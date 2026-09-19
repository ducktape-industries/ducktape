//! Orchestrator e2e over an in-memory message router: N `run()` instances,
//! each with its own keystore + fake effect, wired Send->Deliver exactly the
//! way bin/node's reachability channel will wire them. Proves the whole
//! orchestration pipeline — record gossip -> signed adverts -> converged mesh
//! version -> pairwise handshakes -> ONE apply per node — with no real
//! sockets and no real WireGuard.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use nat_traversal::NodeKey;
use reachability::{
    EndpointResolver as _, InstallReply, MeshEpochEvent, NetstackBackend, ReachabilityCommand,
    ReachabilityConfig, ReachabilityEvent, ReachabilityMsg, Resolution, StaticResolver, SwapReply,
    WireGuardKeypair, binding,
};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use wireguard::effect::{
    FakeWireGuardEffect, FakeWireGuardEffectError, InterfaceConfig, WireGuardEffect,
};
use wireguard::{AllowedIp, Endpoint, PortPolicy, Transport, ValidatorIdentity, X25519PublicKey};

/// `run()` owns its effect; tests need to inspect it afterwards — a shared
/// handle delegating to the fake underneath.
#[derive(Clone, Default)]
struct SharedFake(Arc<Mutex<FakeWireGuardEffect>>);

impl WireGuardEffect for SharedFake {
    type Error = FakeWireGuardEffectError;

    fn create_interface(&mut self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().create_interface()
    }

    fn apply(&mut self, config: &InterfaceConfig) -> Result<(), Self::Error> {
        self.0.lock().unwrap().apply(config)
    }

    fn remove_interface(&mut self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().remove_interface()
    }
}

const CHAIN: &str = "net#e2e";

/// The committed `ducktape:netstack` test fixture; node binaries load a file.
const NETSTACK_COMPONENT: &[u8] = include_bytes!("../../netstack-machine/component.wasm");

fn guest_backend(step_fuel: u64) -> NetstackBackend {
    NetstackBackend::Guest {
        component: NETSTACK_COMPONENT.to_vec(),
        step_fuel,
    }
}

#[derive(Clone)]
struct TestNode {
    signer: PrivateKey,
    identity: ValidatorIdentity,
    octet: u8,
    /// This node's slot in the mesh's routing tables — what a respawn
    /// re-points.
    index: usize,
    cmd: mpsc::Sender<ReachabilityCommand>,
    effect: SharedFake,
    wiring: MeshWiring,
}

/// The shared routing fabric a respawned node rejoins: per-node inbox slots
/// (re-pointed on respawn so deliveries reach the NEW life), the identity
/// tables the router resolves targets against, the collected-event sink,
/// and the delivery filter.
#[derive(Clone)]
struct MeshWiring {
    cmd_slots: Rc<RefCell<Vec<mpsc::Sender<ReachabilityCommand>>>>,
    pks: Vec<commonware_cryptography::ed25519::PublicKey>,
    transports: Vec<commonware_cryptography::ed25519::PublicKey>,
    collected: mpsc::Sender<(usize, ReachabilityEvent)>,
    filter: DeliveryFilter,
}

fn endpoint(policy: &PortPolicy, octet: u8, port: u16, transport: Transport) -> Endpoint {
    Endpoint::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, octet)),
        port,
        transport,
        policy,
    )
    .unwrap()
}

/// Spin up `seeds.len()` planes on the local set, wired through an
/// in-memory router; returns the node handles and the collected non-Send
/// event stream as `(node index, event)`.
fn spawn_mesh(
    local: &LocalSet,
    dir: &std::path::Path,
    seeds: &[u64],
    resolvers: Vec<StaticResolver>,
) -> (Vec<TestNode>, mpsc::Receiver<(usize, ReachabilityEvent)>) {
    let always_up = Arc::new(std::sync::atomic::AtomicBool::new(true));
    spawn_mesh_gated(local, dir, seeds, resolvers, always_up)
}

/// [`spawn_mesh`] with a link gate: while `links_up` is false the router
/// DROPS every `Send` instead of delivering it — the real transport's
/// behavior for a datagram fired before any p2p connection exists (exactly
/// the boot `Retarget` window in `bin/node`).
fn spawn_mesh_gated(
    local: &LocalSet,
    dir: &std::path::Path,
    seeds: &[u64],
    resolvers: Vec<StaticResolver>,
    links_up: Arc<std::sync::atomic::AtomicBool>,
) -> (Vec<TestNode>, mpsc::Receiver<(usize, ReachabilityEvent)>) {
    let filter: DeliveryFilter =
        Rc::new(move |_, _, _| usize::from(links_up.load(std::sync::atomic::Ordering::Relaxed)));
    spawn_mesh_filtered(local, dir, seeds, resolvers, filter)
}

/// How many copies of a routed message to deliver: 0 models a lost datagram
/// (best-effort mesh sends), 2 a transport-level redelivery. Args are
/// `(from node index, to node index, decoded message)`.
type DeliveryFilter = Rc<dyn Fn(usize, usize, &ReachabilityMsg) -> usize>;

/// [`spawn_mesh`] with a per-message [`DeliveryFilter`] on the router — the
/// loss/duplication harness the handshake-retry tests drive.
fn spawn_mesh_filtered(
    local: &LocalSet,
    dir: &std::path::Path,
    seeds: &[u64],
    resolvers: Vec<StaticResolver>,
    filter: DeliveryFilter,
) -> (Vec<TestNode>, mpsc::Receiver<(usize, ReachabilityEvent)>) {
    spawn_mesh_transported(
        local,
        dir,
        seeds,
        resolvers,
        filter,
        vec![],
        None,
        &[],
        &[],
        vec![],
    )
}

/// The full-parameter core: `transport_pks[i]`, when set, is the identity
/// node i's deliveries arrive UNDER (and are routed back to) — the parked
/// standby's lobby shape, where the transport key and the record identity
/// differ. `gossip_ingress` lands in every node's `ReachabilityConfig`.
/// `endpointless[i]` drops node i's `wireguard_advertised` to `None`
/// (change 2 / issue #331's endpoint-less shape); `coordinators_configured[i]`
/// gives node i a non-empty `ReachabilityConfig.coordinators` (the gate the
/// rendezvous fallback checks) — independent knobs so a test can cover
/// "endpoint-less WITHOUT a coordinator" too. `backends[i]` is node i's
/// netstack backend, the native machine when absent.
#[allow(clippy::too_many_arguments)]
fn spawn_mesh_transported(
    local: &LocalSet,
    dir: &std::path::Path,
    seeds: &[u64],
    resolvers: Vec<StaticResolver>,
    filter: DeliveryFilter,
    transport_pks: Vec<Option<commonware_cryptography::ed25519::PublicKey>>,
    gossip_ingress: Option<commonware_cryptography::ed25519::PublicKey>,
    endpointless: &[usize],
    coordinators_configured: &[usize],
    backends: Vec<NetstackBackend>,
) -> (Vec<TestNode>, mpsc::Receiver<(usize, ReachabilityEvent)>) {
    let policy = PortPolicy::production();
    let signers: Vec<PrivateKey> = seeds.iter().map(|s| PrivateKey::from_seed(*s)).collect();
    let pks: Vec<_> = signers.iter().map(|s| s.public_key()).collect();
    let transports: Vec<_> = pks
        .iter()
        .enumerate()
        .map(|(i, pk)| {
            transport_pks
                .get(i)
                .and_then(|t| t.clone())
                .unwrap_or_else(|| pk.clone())
        })
        .collect();
    let (collected_tx, collected_rx) = mpsc::channel(256);

    let mut cmds = Vec::new();
    let mut events = Vec::new();
    for _ in seeds {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (ev_tx, ev_rx) = mpsc::channel(256);
        cmds.push(cmd_tx);
        events.push(Some((cmd_rx, ev_tx, ev_rx)));
    }

    let wiring = MeshWiring {
        cmd_slots: Rc::new(RefCell::new(cmds.clone())),
        pks: pks.clone(),
        transports: transports.clone(),
        collected: collected_tx.clone(),
        filter,
    };
    let mut nodes = Vec::new();
    for (i, signer) in signers.iter().cloned().enumerate() {
        let octet = 10 * (i as u8 + 1);
        let (cmd_rx, ev_tx, ev_rx) = events[i].take().unwrap();
        let effect = SharedFake::default();
        let config = ReachabilityConfig {
            chain_id: CHAIN.into(),
            signer: signer.clone(),
            wireguard_key_file: dir.join(format!("wg-{i}.key")),
            wireguard_port: 51820,
            wireguard_advertised: if endpointless.contains(&i) {
                None
            } else {
                Some(endpoint(&policy, octet, 51820, Transport::Udp))
            },
            control_endpoint: endpoint(&policy, octet, 443, Transport::Tcp),
            coordinators: if coordinators_configured.contains(&i) {
                vec!["203.0.113.53:3478".parse().unwrap()]
            } else {
                vec![]
            },
            port_policy: policy.clone(),
            // every node persists into the shared test dir — respawning
            // from the same dir IS the cold-restart scenario.
            persist_file: Some(dir.join(format!("mesh-{i}.json"))),
            gossip_ingress: gossip_ingress.clone(),
            backend: backends.get(i).cloned().unwrap_or(NetstackBackend::Native),
        };
        let resolver = resolvers
            .get(i)
            .map(|r| StaticResolver(r.0.clone()))
            .unwrap_or_default();
        local.spawn_local(reachability::run(
            config,
            effect.clone(),
            resolver,
            cmd_rx,
            ev_tx,
        ));

        spawn_router(local, wiring.clone(), i, ev_rx);

        nodes.push(TestNode {
            identity: binding::identity_of(&pks[i]),
            signer,
            octet,
            index: i,
            cmd: cmds[i].clone(),
            effect,
            wiring: wiring.clone(),
        });
    }
    (nodes, collected_rx)
}

/// The per-node router pump: this node's `Send` events become `Deliver`
/// commands on the target (as many copies as the filter says); everything
/// else is collected for assertions. Targets match by record identity OR
/// transport identity — exactly like the real mesh, where a send to the
/// lobby key lands on whichever joiner holds that connection. The target's
/// inbox is read from the shared slots at send time, so a respawned life
/// receives instead of a dead channel.
fn spawn_router(
    local: &LocalSet,
    wiring: MeshWiring,
    i: usize,
    mut ev_rx: mpsc::Receiver<ReachabilityEvent>,
) {
    local.spawn_local(async move {
        while let Some(event) = ev_rx.recv().await {
            match event {
                ReachabilityEvent::Send { to, bytes } => {
                    let Some(j) = wiring
                        .pks
                        .iter()
                        .position(|pk| *pk == to)
                        .or_else(|| wiring.transports.iter().position(|pk| *pk == to))
                    else {
                        continue;
                    };
                    let msg = ReachabilityMsg::decode(&bytes).expect("planes send valid frames");
                    let copies = (wiring.filter)(i, j, &msg);
                    let target = wiring.cmd_slots.borrow()[j].clone();
                    let from = wiring.transports[i].clone();
                    for _ in 0..copies {
                        let _ = target
                            .send(ReachabilityCommand::Deliver {
                                from: from.clone(),
                                bytes: bytes.clone(),
                            })
                            .await;
                    }
                }
                other => {
                    let _ = wiring.collected.send((i, other)).await;
                }
            }
        }
    });
}

/// Relaunch node `node.index` as a fresh plane life: the same
/// identity, keystore, and persist file (the crash-restart shape), a fresh
/// effect and command channel, and the advertised address moved to `octet`.
/// Re-points the router's inbox slot so mesh deliveries reach the new life
/// and wires the new life's sends through the same router. The caller
/// retargets (and nudges) the reborn node itself.
fn respawn_node(
    local: &LocalSet,
    dir: &std::path::Path,
    node: &TestNode,
    octet: u8,
    coordinator: bool,
    resolver: StaticResolver,
) -> TestNode {
    let policy = PortPolicy::production();
    let i = node.index;
    let effect = SharedFake::default();
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let (ev_tx, ev_rx) = mpsc::channel(256);
    let config = ReachabilityConfig {
        chain_id: CHAIN.into(),
        signer: node.signer.clone(),
        wireguard_key_file: dir.join(format!("wg-{i}.key")),
        wireguard_port: 51820,
        wireguard_advertised: Some(endpoint(&policy, octet, 51820, Transport::Udp)),
        control_endpoint: endpoint(&policy, octet, 443, Transport::Tcp),
        coordinators: if coordinator {
            vec!["203.0.113.53:3478".parse().unwrap()]
        } else {
            vec![]
        },
        port_policy: policy,
        persist_file: Some(dir.join(format!("mesh-{i}.json"))),
        gossip_ingress: None,
        backend: NetstackBackend::Native,
    };
    local.spawn_local(reachability::run(
        config,
        effect.clone(),
        resolver,
        cmd_rx,
        ev_tx,
    ));
    node.wiring.cmd_slots.borrow_mut()[i] = cmd_tx.clone();
    spawn_router(local, node.wiring.clone(), i, ev_rx);
    TestNode {
        signer: node.signer.clone(),
        identity: node.identity,
        octet,
        index: i,
        cmd: cmd_tx,
        effect,
        wiring: node.wiring.clone(),
    }
}

async fn retarget_all(
    nodes: &[TestNode],
    members: &[usize],
    standbys: &[usize],
    epoch: u64,
    view: u64,
) {
    let pks: Vec<_> = members
        .iter()
        .map(|i| nodes[*i].signer.public_key())
        .collect();
    let standby_pks: Vec<_> = standbys
        .iter()
        .map(|i| nodes[*i].signer.public_key())
        .collect();
    for i in members.iter().chain(standbys.iter()) {
        nodes[*i]
            .cmd
            .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                epoch,
                members: pks.clone(),
                standbys: standby_pks.clone(),
                current_view: view,
            }))
            .await
            .unwrap();
    }
}

/// How long a whole-mesh convergence may take before the test calls it hung.
///
/// A BACKSTOP, not a synchronizer: every wait below is driven by the
/// plane's own events and exits the moment they arrive, so this bound
/// only turns a hang into a failure. Kept generous because the cost of a
/// generous backstop on a healthy run is zero and the cost of a tight one is a
/// red CI on a busy machine.
const CONVERGE_BUDGET: Duration = Duration::from_secs(30);

/// What a convergence drain saw: the `MeshReady` version per node and every
/// `EpochFailed` on the way, in order.
struct Converged {
    versions: HashMap<usize, Vec<u8>>,
    epoch_failures: Vec<(usize, String)>,
}

/// Drain collected events until every node in `want` has emitted
/// `TunnelsApplied` for `epoch`. `EpochFailed` is recorded for the caller to
/// judge; `PeerFailed` is never legitimate on the way to convergence — it
/// means a retry replayed or a duplicate was mistaken for a violation.
async fn drain_until_applied(
    collected: &mut mpsc::Receiver<(usize, ReachabilityEvent)>,
    want: &[usize],
    epoch: u64,
) -> Converged {
    let mut versions = HashMap::new();
    let mut epoch_failures = Vec::new();
    let mut applied: Vec<usize> = Vec::new();
    tokio::time::timeout(CONVERGE_BUDGET, async {
        while applied.len() < want.len() {
            let (i, event) = collected.recv().await.expect("event stream open");
            match event {
                ReachabilityEvent::MeshReady {
                    epoch: e, version, ..
                } if e == epoch => {
                    versions.insert(i, version.0.to_vec());
                }
                ReachabilityEvent::TunnelsApplied { epoch: e, .. } if e == epoch => {
                    applied.push(i);
                }
                ReachabilityEvent::EpochFailed { reason, .. } => {
                    epoch_failures.push((i, reason));
                }
                ReachabilityEvent::PeerFailed { reason, .. } => {
                    panic!("peer failed on node {i}: {reason}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("mesh converged in time");
    Converged {
        versions,
        epoch_failures,
    }
}

/// [`drain_until_applied`] for a HEALTHY convergence, which emits no
/// failure event at all; returns the `MeshReady` versions seen.
async fn await_applied(
    collected: &mut mpsc::Receiver<(usize, ReachabilityEvent)>,
    want: &[usize],
    epoch: u64,
) -> HashMap<usize, Vec<u8>> {
    let converged = drain_until_applied(collected, want, epoch).await;
    assert!(
        converged.epoch_failures.is_empty(),
        "epoch failed: {:?}",
        converged.epoch_failures
    );
    converged.versions
}

/// bin/node's nudge ticker, at test cadence, for every node.
fn spawn_nudgers(local: &LocalSet, nodes: &[TestNode]) {
    for node in nodes {
        let cmd = node.cmd.clone();
        local.spawn_local(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            // the same discipline `bin/node`'s nudger already runs under, and
            // for the same reason. The default is `Burst`: a starved test
            // (several single-threaded `LocalSet`s on a loaded box) comes back
            // to a pile of missed 50 ms ticks and fires them ALL, and the nudge
            // storm then outruns the delivery tasks it shares a thread with —
            // convergence stops making progress instead of merely slowing down.
            // Measured: 6 s idle, still unconverged at 60 s under 40 busy loops.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if cmd.send(ReachabilityCommand::Nudge).await.is_err() {
                    break;
                }
            }
        });
    }
}

fn ula(identity: ValidatorIdentity) -> AllowedIp {
    AllowedIp::new(
        std::net::IpAddr::V6(wireguard::ula_v6_member_addr(CHAIN, identity)),
        128,
    )
    .expect("a member /128 is a valid route")
}

#[tokio::test]
async fn three_member_mesh_converges_and_applies() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            // node 0 resolves node 2 through a "punched" path; every other
            // pairing rides advertised endpoints.
            let punched: std::net::SocketAddr = "203.0.113.7:40001".parse().unwrap();
            let identity_of_seed =
                |seed: u64| binding::identity_of(&PrivateKey::from_seed(seed).public_key());
            let mut r0 = StaticResolver::default();
            r0.0.insert(NodeKey(identity_of_seed(3).0), Resolution::Punched(punched));
            let (nodes, mut collected) = spawn_mesh(
                &local,
                dir.path(),
                &[1, 2, 3],
                vec![r0, StaticResolver::default(), StaticResolver::default()],
            );

            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            let versions = await_applied(&mut collected, &[0, 1, 2], 1).await;

            // one content-derived mesh version, byte-identical on every node.
            assert_eq!(versions.len(), 3);
            assert_eq!(versions[&0], versions[&1]);
            assert_eq!(versions[&0], versions[&2]);

            let ifname = binding::interface_name(CHAIN);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "node {i}: one interface");
                assert_eq!(fake.applied.len(), 1, "node {i}: one apply");
                let config = &fake.applied[0];
                assert_eq!(config.name, ifname);
                assert_eq!(config.listen_port, 51820);
                // interface address: exactly this node's identity-hash /128.
                assert_eq!(config.addresses, vec![ula(node.identity)]);
                // the persisted keystore is what the interface runs.
                let (keypair, generated) =
                    WireGuardKeypair::load_or_generate(&dir.path().join(format!("wg-{i}.key")))
                        .unwrap();
                assert!(!generated, "node {i}: run() created the key");
                assert_eq!(config.private_key, keypair.private_key_bytes());
                // both peers present, each routing ONLY its own /128 and
                // keyed by ITS persisted public key.
                assert_eq!(config.peers.len(), 2);
                for (j, peer_node) in nodes.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    let (peer_keys, _) =
                        WireGuardKeypair::load_or_generate(&dir.path().join(format!("wg-{j}.key")))
                            .unwrap();
                    let entry = config
                        .peers
                        .iter()
                        .find(|p| p.allowed_ips == vec![ula(peer_node.identity)])
                        .unwrap_or_else(|| panic!("node {i}: no peer entry for node {j}"));
                    assert_eq!(entry.wireguard_public_key, peer_keys.public_key());
                    let expected = if i == 0 && j == 2 {
                        punched
                    } else {
                        format!("8.8.8.{}:51820", peer_node.octet).parse().unwrap()
                    };
                    assert_eq!(entry.endpoint, Some(expected), "node {i} -> node {j}");
                }
            }

            // shutdown tears the live interface down.
            nodes[0]
                .cmd
                .send(ReachabilityCommand::Shutdown)
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if nodes[0].effect.0.lock().unwrap().remove_calls == 1 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("shutdown removed the interface");
        })
        .await;
}

#[tokio::test]
async fn epoch_cutover_reconfigures_the_live_interface_to_the_reduced_mesh() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);

            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            await_applied(&mut collected, &[0, 1, 2], 1).await;

            // epoch 2: node 2 departs.
            retarget_all(&nodes, &[0, 1], &[], 2, 20).await;
            await_applied(&mut collected, &[0, 1], 2).await;

            for i in [0usize, 1] {
                let fake = nodes[i].effect.0.lock().unwrap();
                assert_eq!(
                    fake.remove_calls, 0,
                    "node {i}: the cutover reconfigures in place — epoch 1's interface survives"
                );
                assert_eq!(
                    fake.create_calls, 1,
                    "node {i}: one interface across both epochs"
                );
                assert_eq!(fake.applied.len(), 2);
                let second = &fake.applied[1];
                assert_eq!(second.peers.len(), 1, "node {i}: reduced mesh");
                let other = &nodes[1 - i];
                assert_eq!(second.peers[0].allowed_ips, vec![ula(other.identity)]);
            }
        })
        .await;
}

#[tokio::test]
async fn single_member_mesh_and_stranger_traffic_are_inert() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1], vec![]);

            retarget_all(&nodes, &[0], &[], 1, 10).await;
            await_applied(&mut collected, &[0], 1).await;
            {
                let fake = nodes[0].effect.0.lock().unwrap();
                // A peerless mesh still brings the interface up (own /128, no
                // peers): the per-use media planes bind it, so a single-member
                // network — every fresh desktop workspace — can huddle solo.
                assert_eq!(
                    fake.create_calls, 1,
                    "a peerless mesh brings up its own interface"
                );
                assert_eq!(fake.applied.len(), 1, "one apply");
                assert!(
                    fake.applied[0].peers.is_empty(),
                    "no peer tunnels on a peerless mesh"
                );
            }

            // a well-formed message from a key outside the member set is
            // refused loudly, never processed.
            let stranger = PrivateKey::from_seed(99);
            nodes[0]
                .cmd
                .send(ReachabilityCommand::Deliver {
                    from: stranger.public_key(),
                    bytes: b"junk".to_vec(),
                })
                .await
                .unwrap();
            let reason = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some((_, ReachabilityEvent::PeerFailed { reason, .. })) =
                        collected.recv().await
                    {
                        break reason;
                    }
                }
            })
            .await
            .expect("stranger traffic surfaced");
            assert!(reason.contains("non-participant"), "{reason}");
        })
        .await;
}

/// Establishment is asynchronous now (`bind` returns before discovery), so
/// tests that need a live registration wait, bounded, for `Ready` first.
async fn established(resolver: &reachability::NatResolver) -> std::net::SocketAddr {
    let mut status = resolver.status().expect("resolver has coordinators");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let reachability::RendezvousStatus::Ready { reflexive } = *status.borrow_and_update()
            {
                return reflexive;
            }
            status.changed().await.expect("establish task alive");
        }
    })
    .await
    .expect("rendezvous must establish against a live coordinator")
}

/// The production resolver against a REAL coordinator + two real UDP
/// clients on loopback: register, lookup, simultaneous-open punch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nat_resolver_punches_over_loopback() {
    let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let coord_addr = coord_sock.local_addr().unwrap();
    tokio::spawn(nat_traversal::client::run_coordinator(
        nat_traversal::NatSocket::Owned(coord_sock),
        nat_traversal::AuthPolicy::Public,
    ));

    let signer_a = PrivateKey::from_seed(490);
    let signer_b = PrivateKey::from_seed(491);
    let key_a = node_key_of(&signer_a.public_key());
    let key_b = node_key_of(&signer_b.public_key());
    let mut a = reachability::NatResolver::bind(key_a, vec![coord_addr], (signer_a, None))
        .await
        .unwrap();
    let mut b = reachability::NatResolver::bind(key_b, vec![coord_addr], (signer_b, None))
        .await
        .unwrap();
    established(&a).await;
    established(&b).await;
    assert!(a.reflexive().is_some(), "bind discovered the reflexive");

    let dummy: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let (ra, rb) = tokio::join!(a.resolve(key_b, dummy), b.resolve(key_a, dummy));
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    // loopback "NAT": the punched address IS the peer's socket, and both
    // sides punched through.
    match (ra, rb) {
        (Resolution::Punched(to_b), Resolution::Punched(to_a)) => {
            assert_eq!(to_b, b.reflexive().unwrap());
            assert_eq!(to_a, a.reflexive().unwrap());
        }
        other => panic!("expected punched/punched, got {other:?}"),
    }
}

/// The REAL `send_datagram_and_recv` over loopback UDP: the addressed peer's
/// reply is returned (matched by src) while a STRANGER's datagram arriving
/// mid-wait is forwarded to the datagram sink instead of being swallowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolver_datagram_roundtrip_over_loopback() {
    let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let coord_addr = coord_sock.local_addr().unwrap();
    tokio::spawn(nat_traversal::client::run_coordinator(
        nat_traversal::NatSocket::Owned(coord_sock),
        nat_traversal::AuthPolicy::Public,
    ));

    let signer = PrivateKey::from_seed(492);
    let key = node_key_of(&signer.public_key());
    let client = nat_traversal::NatClient::bind(key, vec![coord_addr], signer, None)
        .await
        .unwrap();
    let (sink_tx, mut sink_rx) = mpsc::channel(8);
    let mut resolver = reachability::NatResolver::from_client_with_datagram_sink(
        client,
        reachability::RENDEZVOUS_KEEPALIVE,
        Some(sink_tx),
        reachability::CarryingPeers::default(),
    );
    established(&resolver).await;

    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let stranger = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stranger_addr = stranger.local_addr().unwrap();

    // the peer echoes an ack — but only after the stranger's datagram has hit
    // the resolver socket mid-wait, the one that must be sinked, not eaten.
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        let (n, resolver_addr) = peer.recv_from(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"intro");
        stranger.send_to(b"unrelated", resolver_addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        peer.send_to(b"intro-ack", resolver_addr).await.unwrap();
    });

    let reply = resolver
        .send_datagram_and_recv(peer_addr, b"intro".to_vec(), Duration::from_secs(5))
        .await
        .expect("the peer's reply is returned");
    assert_eq!(reply, b"intro-ack");

    let (src, bytes) = tokio::time::timeout(Duration::from_secs(2), sink_rx.recv())
        .await
        .expect("bounded")
        .expect("sink stays open");
    assert_eq!(src, stranger_addr, "the sinked datagram is the stranger's");
    assert_eq!(bytes, b"unrelated");
}

/// A `NodeKey` whose bytes ARE the ed25519 public key — the subject a signed
/// coordinator request proves possession of.
fn node_key_of(pk: &commonware_cryptography::ed25519::PublicKey) -> NodeKey {
    let mut b = [0u8; 32];
    b.copy_from_slice(pk.as_ref());
    NodeKey(b)
}

/// A REAL private (genesis-gated) coordinator ADMITS an authenticated node
/// whose every coordinator request satisfies the `AuthPolicy::Private` gate:
/// a genesis member (admitted by membership) and a non-genesis joiner carrying
/// a genesis-minted cap (admitted by capability) both bind successfully —
/// `NatResolver::bind` runs an authenticated `BindRequest` + `Register`, both
/// self-subject (PoP proves possession of the requesting key), so the gate
/// passes and the reflexive is discovered.
///
/// Scope: this asserts admission at the bind boundary. The full cross-peer
/// rendezvous/punch under a private coordinator is proven by
/// [`private_coordinator_cross_peer_punch`] below — the coordinator
/// authenticates the request CALLER (its PoP-signing identity), so a `Lookup`
/// for a DIFFERENT peer's key authenticates fine and the hole-punch completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_coordinator_admits_authenticated_bind() {
    let g = PrivateKey::from_seed(500);
    let member = PrivateKey::from_seed(501);
    let joiner = PrivateKey::from_seed(502); // NOT genesis; admitted by a cap
    let policy = nat_traversal::AuthPolicy::Private {
        genesis_set: vec![g.public_key(), member.public_key()],
    };

    let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let coord_addr = coord_sock.local_addr().unwrap();
    tokio::spawn(nat_traversal::run_coordinator(
        nat_traversal::NatSocket::Owned(coord_sock),
        policy,
    ));

    // a genesis member: admitted by membership, no cap needed.
    let member_key = node_key_of(&member.public_key());
    let m = reachability::NatResolver::bind(member_key, vec![coord_addr], (member.clone(), None))
        .await
        .expect("a genesis member's authenticated bind is admitted");
    established(&m).await;
    assert!(
        m.reflexive().is_some(),
        "the member discovered its reflexive through the private coordinator"
    );

    // a non-genesis joiner carrying a genesis-minted cap: admitted by the cap.
    let joiner_key = node_key_of(&joiner.public_key());
    let cap = nat_traversal::mint_coord_cap(&g, joiner_key, nat_traversal::now_secs() + 3600);
    let j =
        reachability::NatResolver::bind(joiner_key, vec![coord_addr], (joiner.clone(), Some(cap)))
            .await
            .expect("a capped joiner's authenticated bind is admitted");
    established(&j).await;
    assert!(
        j.reflexive().is_some(),
        "the capped joiner discovered its reflexive through the private coordinator"
    );
}

/// A resolver with NO cap and a NON-genesis key is REFUSED by a private
/// coordinator: its `BindRequest` (valid PoP, but neither a genesis member
/// nor cap-bearing) is silently dropped by the admission gate. Denial is
/// deliberately indistinguishable from a dark network on the wire (the gate
/// drops, never answers), so with background establishment the refusal shows
/// up as NEVER-READY: the resolver keeps retrying and no reflexive ever
/// lands. A member against the same coordinator establishes promptly —
/// proving it is the missing credential, not the transport, that denies the
/// outsider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_coordinator_denies_uncredentialed_bind() {
    let g = PrivateKey::from_seed(600);
    let member = PrivateKey::from_seed(601);
    let outsider = PrivateKey::from_seed(602); // NOT in the genesis set, no cap
    let policy = nat_traversal::AuthPolicy::Private {
        genesis_set: vec![g.public_key(), member.public_key()],
    };

    let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let coord_addr = coord_sock.local_addr().unwrap();
    tokio::spawn(nat_traversal::run_coordinator(
        nat_traversal::NatSocket::Owned(coord_sock),
        policy,
    ));

    // the outsider authenticates (valid PoP) but carries no cap and is not a
    // genesis member: every request, BindRequest included, is dropped by the
    // admission gate, so establishment can never discover a reflexive.
    let outsider_key = node_key_of(&outsider.public_key());
    let denied =
        reachability::NatResolver::bind(outsider_key, vec![coord_addr], (outsider.clone(), None))
            .await
            .expect("the local socket binds; admission shows up as never-Ready");

    // control: a credentialed member establishes against the SAME coordinator
    // while the outsider is still being refused — the transport works.
    let member_key = node_key_of(&member.public_key());
    let ok = reachability::NatResolver::bind(member_key, vec![coord_addr], (member.clone(), None))
        .await
        .expect("a genesis member's socket binds");
    established(&ok).await;

    // one full discovery attempt (3s) has certainly concluded by now — the
    // member above establishes in milliseconds on loopback; give the outsider
    // a further beat and hold that it never became Ready.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        denied.reflexive(),
        None,
        "an uncredentialed (non-genesis, uncapped) node must never establish rendezvous"
    );
}

/// The full cross-peer rendezvous under a REAL private (genesis-gated)
/// coordinator: two authorized nodes A and B (each holding a genesis-minted
/// cap) bind + register, then A resolves B's DIFFERENT key and B resolves A's,
/// and both simultaneous-open punches complete over loopback. Under the OLD
/// code the coordinator authenticated a `Lookup` against the LOOKED-UP key, so
/// A's PoP (signed with A's own key) failed against B's key and the lookup was
/// silently dropped — this resolve timed out. Authenticating the CALLER fixes
/// it: the core rendezvous path works under a private coordinator.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_coordinator_cross_peer_punch() {
    let g = PrivateKey::from_seed(700);
    let a_signer = PrivateKey::from_seed(701);
    let b_signer = PrivateKey::from_seed(702);
    let policy = nat_traversal::AuthPolicy::Private {
        genesis_set: vec![g.public_key()],
    };

    let coord_sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let coord_addr = coord_sock.local_addr().unwrap();
    tokio::spawn(nat_traversal::run_coordinator(
        nat_traversal::NatSocket::Owned(coord_sock),
        policy,
    ));

    let a_key = node_key_of(&a_signer.public_key());
    let b_key = node_key_of(&b_signer.public_key());
    let a_cap = nat_traversal::mint_coord_cap(&g, a_key, nat_traversal::now_secs() + 3600);
    let b_cap = nat_traversal::mint_coord_cap(&g, b_key, nat_traversal::now_secs() + 3600);

    let mut a = reachability::NatResolver::bind(a_key, vec![coord_addr], (a_signer, Some(a_cap)))
        .await
        .expect("A's authenticated bind is admitted");
    let mut b = reachability::NatResolver::bind(b_key, vec![coord_addr], (b_signer, Some(b_cap)))
        .await
        .expect("B's authenticated bind is admitted");
    established(&a).await;
    established(&b).await;
    assert!(a.reflexive().is_some() && b.reflexive().is_some());

    // Cross-peer resolve on both sides: A looks up B's key, B looks up A's.
    let dummy: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
    let (ra, rb) = tokio::join!(a.resolve(b_key, dummy), b.resolve(a_key, dummy));
    match (ra.unwrap(), rb.unwrap()) {
        (Resolution::Punched(to_b), Resolution::Punched(to_a)) => {
            assert_eq!(to_b, b.reflexive().unwrap());
            assert_eq!(to_a, a.reflexive().unwrap());
        }
        other => panic!("expected punched/punched under a private coordinator, got {other:?}"),
    }
}

/// An empty coordinator set degrades to pass-through resolution.
#[tokio::test]
async fn nat_resolver_without_coordinators_is_pass_through() {
    let signer = PrivateKey::from_seed(493);
    let key = node_key_of(&signer.public_key());
    let mut r = reachability::NatResolver::bind(key, vec![], (signer, None))
        .await
        .unwrap();
    assert_eq!(r.reflexive(), None);
    let advertised: std::net::SocketAddr = "8.8.8.10:51820".parse().unwrap();
    assert_eq!(
        r.resolve(NodeKey([2; 32]), advertised).await.unwrap(),
        Resolution::Advertised
    );
}

/// the boot race: every node's `Retarget` record fan-out fires before the
/// transport has any live connection, so BOTH sides of every link lose their
/// initial `EndpointRecord` — `on_record`'s first-contact heal never fires
/// and, without nudges, the epoch stalls in record gossip forever. periodic
/// `Nudge` commands (bin/node's ticker) re-offer the stored gossip once the
/// links are up, and the mesh must then converge to a full apply.
#[tokio::test]
async fn boot_window_record_loss_heals_by_nudge() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) = spawn_mesh_gated(
                &local,
                dir.path(),
                &[1, 2],
                vec![StaticResolver::default(), StaticResolver::default()],
                links_up.clone(),
            );

            // both boot Retargets fire into the dead transport.
            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            tokio::task::yield_now().await;

            // the links come up AFTER the initial fan-out was lost.
            links_up.store(true, std::sync::atomic::Ordering::Relaxed);

            spawn_nudgers(&local, &nodes);

            let versions = await_applied(&mut collected, &[0, 1], 1).await;
            assert_eq!(versions[&0], versions[&1]);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "node {i}: one interface");
                assert_eq!(fake.applied.len(), 1, "node {i}: one apply");
                assert_eq!(fake.applied[0].peers.len(), 1);
            }
        })
        .await;
}

/// Drop the FIRST message matched by `hit`, deliver everything else — the
/// single-shot handshake-loss scenarios.
fn drop_first(hit: fn(&ReachabilityMsg) -> bool) -> (DeliveryFilter, Rc<Cell<bool>>) {
    let dropped = Rc::new(Cell::new(false));
    let flag = dropped.clone();
    let filter: DeliveryFilter = Rc::new(move |_, _, msg| {
        if !flag.get() && hit(msg) {
            flag.set(true);
            0
        } else {
            1
        }
    });
    (filter, dropped)
}

/// Run a 2-node mesh whose router applies `filter`, with nudge tickers
/// running, and require full convergence with zero failure events — the
/// shared body of the handshake-retry tests.
async fn converges_despite(filter: DeliveryFilter) {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) =
                spawn_mesh_filtered(&local, dir.path(), &[1, 2], vec![], filter);
            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_applied(&mut collected, &[0, 1], 1).await;
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.applied.len(), 1, "node {i}: exactly one apply");
                assert_eq!(fake.applied[0].peers.len(), 1);
            }
        })
        .await;
}

/// A lost `TunnelUpgradeRequest` used to stall its pair for the whole epoch
/// (handshake messages were single-shot): the initiator waits for a response
/// that can never come. The nudge ticker must re-offer the stored request
/// verbatim and the mesh must still converge, with no failure noise.
#[tokio::test]
async fn dropped_handshake_request_heals_by_nudge() {
    let (filter, dropped) = drop_first(|m| matches!(m, ReachabilityMsg::Request(_)));
    converges_despite(filter).await;
    assert!(dropped.get(), "a request was actually dropped");
}

/// A lost `TunnelUpgradeResponse` strands BOTH sides: the initiator awaits
/// the response, the responder awaits an ack for it. The initiator's nudged
/// request re-offer must elicit the responder's STORED response (never a
/// re-signed one — the replay cache and the ack's response_hash both pin it).
#[tokio::test]
async fn dropped_handshake_response_heals_by_nudge() {
    let (filter, dropped) = drop_first(|m| matches!(m, ReachabilityMsg::Response(_)));
    converges_despite(filter).await;
    assert!(dropped.get(), "a response was actually dropped");
}

/// A lost `TunnelUpgradeAck` is the asymmetric case: the initiator already
/// validated its plan and applies, while the responder waits forever. The
/// responder's nudged response re-offer must make the completed initiator
/// re-send its stored ack verbatim.
#[tokio::test]
async fn dropped_handshake_ack_heals_by_nudge() {
    let (filter, dropped) = drop_first(|m| matches!(m, ReachabilityMsg::Ack(_)));
    converges_despite(filter).await;
    assert!(dropped.get(), "an ack was actually dropped");
}

/// Transport-level redelivery: EVERY message arrives twice. Duplicates must
/// be recognized as such (by hash, answered by verbatim re-sends at most) —
/// never re-validated into the shared per-epoch `ReplayCache`, never
/// mistaken for protocol violations, and never re-signed into a second
/// response that desynchronizes the ack.
#[tokio::test]
async fn duplicated_delivery_is_tolerated_end_to_end() {
    converges_despite(Rc::new(|_, _, _| 2)).await;
}

/// A member that has verified its own view must still answer a peer that has
/// not — the phase-A exchange is one-shot, and losing one half of it used to
/// wedge that peer for the whole epoch.
///
/// The shape is the live one. Node 0's advert reaches node 1 late (or not at
/// all): node 1's advert still reaches node 0, so node 0 assembles the full
/// set, verifies, and stops gossiping records and adverts — while node 1 sits
/// one advert short, nudging forever at a node that now drops everything it
/// sends. On a real network the loss is not hypothetical: a member learns how
/// to DIAL a just-promoted joiner from the very record that completes its own
/// assembly, so its reply goes out microseconds before that link exists.
///
/// The heal is what closes it: a peer still gossiping phase-A at a verified
/// node is a peer missing our half, so the next nudge sends it back.
#[tokio::test]
async fn a_verified_member_still_answers_a_peer_that_is_behind() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            // node 0's first four adverts toward node 1 never arrive; every
            // other message flows. Four is past the one-shot fan-out AND the
            // `first_contact` reply, so only a heal can deliver the fifth.
            let dropped: Rc<Cell<usize>> = Rc::default();
            let filter: DeliveryFilter = Rc::new(move |from, to, msg| {
                let advert_to_the_straggler =
                    (from, to) == (0, 1) && matches!(msg, ReachabilityMsg::Advert(_));
                if !advert_to_the_straggler {
                    return 1;
                }
                let n = dropped.get() + 1;
                dropped.set(n);
                usize::from(n > 4)
            });
            let (nodes, mut collected) =
                spawn_mesh_filtered(&local, dir.path(), &[1, 2], vec![], filter);
            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            // the straggler applies too, on the same mesh version.
            let versions = await_applied(&mut collected, &[0, 1], 1).await;
            assert_eq!(versions[&0], versions[&1]);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.applied.len(), 1, "node {i}: exactly one apply");
                assert_eq!(fake.applied[0].peers.len(), 1, "node {i}: full mesh");
            }
        })
        .await;
}

/// The kitchen sink: a 3-node mesh where the FIRST copy of every
/// (sender, receiver, message kind) is lost — record, advert, request,
/// response, and ack alike. Nudge re-offers must heal every stage.
#[tokio::test]
async fn every_message_kind_dropped_once_still_converges() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            type SeenKey = (usize, usize, &'static str);
            let seen: Rc<RefCell<HashSet<SeenKey>>> = Rc::default();
            let filter: DeliveryFilter = Rc::new(move |from, to, msg| {
                let kind = match msg {
                    ReachabilityMsg::Record(_) => "record",
                    ReachabilityMsg::Advert(_) => "advert",
                    ReachabilityMsg::Request(_) => "request",
                    ReachabilityMsg::Response(_) => "response",
                    ReachabilityMsg::Ack(_) => "ack",
                };
                usize::from(!seen.borrow_mut().insert((from, to, kind)))
            });
            let (nodes, mut collected) =
                spawn_mesh_filtered(&local, dir.path(), &[1, 2, 3], vec![], filter);
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            let versions = await_applied(&mut collected, &[0, 1, 2], 1).await;
            assert_eq!(versions[&0], versions[&1]);
            assert_eq!(versions[&0], versions[&2]);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.applied.len(), 1, "node {i}: exactly one apply");
                assert_eq!(fake.applied[0].peers.len(), 2);
            }
        })
        .await;
}

/// The joiner topology: node 2 has a live link ONLY to node 0 (its inviter's
/// ingress) — nodes 1 and 2 have no transport path in either direction, the
/// exact shape of a coordinated-only joiner parked through one ephemeral
/// ingress. Gossip must relay through node 0 (records, adverts, and the 1<->2
/// handshake alike) and the full mesh must still converge on every node.
#[tokio::test]
async fn star_topology_relays_gossip_through_the_hub() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let filter: DeliveryFilter =
                Rc::new(|from, to, _| usize::from(!matches!((from, to), (1, 2) | (2, 1))));
            let (nodes, mut collected) =
                spawn_mesh_filtered(&local, dir.path(), &[1, 2, 3], vec![], filter);
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            let versions = await_applied(&mut collected, &[0, 1, 2], 1).await;
            assert_eq!(versions[&0], versions[&1]);
            assert_eq!(versions[&0], versions[&2]);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.applied.len(), 1, "node {i}: exactly one apply");
                assert_eq!(fake.applied[0].peers.len(), 2, "node {i}: full mesh");
            }
        })
        .await;
}

/// With relaying, a record's authenticity comes from its OWNER's signature,
/// not the delivering link: a member forwarding a record whose signature
/// does not verify (tampered in flight, or outright forged) is refused
/// loudly, attributed to the DELIVERING member.
#[tokio::test]
async fn forged_relayed_record_is_refused() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2], vec![]);
            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;

            let policy = PortPolicy::production();
            let forged = wireguard::SignedEndpointRecord {
                record: wireguard::EndpointRecord {
                    namespace: CHAIN.into(),
                    epoch: 1,
                    valset_root: wireguard::Root([1; 32]),
                    admission_root: wireguard::AdmissionRoot([2; 32]),
                    validator_identity: nodes[1].identity,
                    wireguard_public_key: wireguard::X25519PublicKey([4; 32]),
                    control_endpoint: endpoint(&policy, 20, 443, Transport::Tcp),
                    wireguard_endpoint: Some(endpoint(&policy, 20, 51820, Transport::Udp)),
                    nonce: 9,
                },
                signature: wireguard::SignatureBytes(vec![0; 64]),
            };
            nodes[0]
                .cmd
                .send(ReachabilityCommand::Deliver {
                    from: nodes[1].signer.public_key(),
                    bytes: ReachabilityMsg::Record(forged).encode(),
                })
                .await
                .unwrap();

            let (peer, reason) = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some((0, ReachabilityEvent::PeerFailed { peer, reason })) =
                        collected.recv().await
                    {
                        break (peer, reason);
                    }
                }
            })
            .await
            .expect("forged record surfaced");
            assert!(reason.contains("record signature invalid"), "{reason}");
            assert_eq!(peer, nodes[1].signer.public_key());
        })
        .await;
}

/// Drain collected events until every node in `want` has emitted
/// `MeshRestored`; returns node -> (restored epoch, restored peer count).
/// Any failure event during a restore window is a bug.
async fn await_restored(
    collected: &mut mpsc::Receiver<(usize, ReachabilityEvent)>,
    want: &[usize],
) -> HashMap<usize, (u64, usize)> {
    let mut restored = HashMap::new();
    tokio::time::timeout(CONVERGE_BUDGET, async {
        while restored.len() < want.len() {
            let (i, event) = collected.recv().await.expect("event stream open");
            match event {
                ReachabilityEvent::MeshRestored { epoch, peers, .. } if want.contains(&i) => {
                    restored.insert(i, (epoch, peers));
                }
                ReachabilityEvent::RestoreFailed { reason } => {
                    panic!("restore failed on node {i}: {reason}");
                }
                ReachabilityEvent::PersistFailed { reason } => {
                    panic!("persist failed on node {i}: {reason}");
                }
                ReachabilityEvent::EpochFailed { reason, .. } => {
                    panic!("epoch failed on node {i}: {reason}");
                }
                ReachabilityEvent::PeerFailed { reason, .. } => {
                    panic!("peer failed on node {i}: {reason}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("mesh restored in time");
    restored
}

/// THE cold-restart brick, healed from disk alone: converge a mesh in one
/// process life, then respawn every plane from the same directory
/// with the router gate DOWN — the exact restart topology where no TCP link
/// exists, so no gossip can flow and (before persistence) nothing could ever
/// apply. Every node must re-apply the persisted mesh purely locally, with
/// FRESH resolver-provided endpoints, and once links return the boot epoch's
/// own assembly must replace the restored interface.
#[tokio::test]
async fn cold_restart_restores_the_mesh_with_no_transport_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let ifname = binding::interface_name(CHAIN);

    // life 1: a normal three-member convergence persists each node's mesh.
    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
                retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
                await_applied(&mut collected, &[0, 1, 2], 1).await;
            })
            .await;
        // dropping the LocalSet kills every run() mid-flight — a process
        // exit, tunnels gone with it.
    }
    for i in 0..3 {
        assert!(
            dir.path().join(format!("mesh-{i}.json")).exists(),
            "node {i} persisted its mesh"
        );
    }

    // life 2: same directory, ZERO transport. node 0 re-resolves node 1
    // through a fresh "punched" path — the persisted world never contained
    // this address, so seeing it applied proves resolution ran fresh at
    // boot rather than replaying stale observations.
    let local = LocalSet::new();
    local
        .run_until(async {
            let fresh_punch: std::net::SocketAddr = "203.0.113.99:40009".parse().unwrap();
            let identity_of_seed =
                |seed: u64| binding::identity_of(&PrivateKey::from_seed(seed).public_key());
            let mut r0 = StaticResolver::default();
            r0.0.insert(
                NodeKey(identity_of_seed(2).0),
                Resolution::Punched(fresh_punch),
            );
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) = spawn_mesh_gated(
                &local,
                dir.path(),
                &[1, 2, 3],
                vec![r0, StaticResolver::default(), StaticResolver::default()],
                links_up.clone(),
            );
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;

            let restored = await_restored(&mut collected, &[0, 1, 2]).await;
            for i in 0..3 {
                assert_eq!(restored[&i], (1, 2), "node {i}: full mesh from epoch 1");
            }
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.applied.len(), 1, "node {i}: the restore apply");
                let config = &fake.applied[0];
                assert_eq!(config.name, ifname);
                assert_eq!(config.addresses, vec![ula(node.identity)]);
                for (j, peer_node) in nodes.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    let (peer_keys, _) =
                        WireGuardKeypair::load_or_generate(&dir.path().join(format!("wg-{j}.key")))
                            .unwrap();
                    let entry = config
                        .peers
                        .iter()
                        .find(|p| p.allowed_ips == vec![ula(peer_node.identity)])
                        .unwrap_or_else(|| panic!("node {i}: no peer entry for node {j}"));
                    assert_eq!(entry.wireguard_public_key, peer_keys.public_key());
                    let expected = if i == 0 && j == 1 {
                        fresh_punch
                    } else {
                        format!("8.8.8.{}:51820", peer_node.octet).parse().unwrap()
                    };
                    assert_eq!(entry.endpoint, Some(expected), "node {i} -> node {j}");
                }
            }

            // the restored mesh is a bootstrap, not the destination: once
            // links exist, the boot epoch assembles live and reconfigures
            // the interface onto its verified base.
            spawn_nudgers(&local, &nodes);
            links_up.store(true, std::sync::atomic::Ordering::Relaxed);
            await_applied(&mut collected, &[0, 1, 2], 1).await;
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(
                    fake.remove_calls, 0,
                    "node {i}: the live apply reconfigures the restored interface in place"
                );
                assert_eq!(fake.applied.len(), 2, "node {i}: restore, then live apply");
            }
        })
        .await;
}

/// A boot whose member set shrank since the mesh was persisted restores
/// only the intersection: the departed member's tunnel is never applied.
#[tokio::test]
async fn cold_restart_filters_departed_members() {
    let dir = tempfile::tempdir().unwrap();

    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
                retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
                await_applied(&mut collected, &[0, 1, 2], 1).await;
            })
            .await;
    }

    let local = LocalSet::new();
    local
        .run_until(async {
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) =
                spawn_mesh_gated(&local, dir.path(), &[1, 2, 3], vec![], links_up);
            // the resumed epoch dropped node 2.
            retarget_all(&nodes, &[0, 1], &[], 2, 20).await;

            let restored = await_restored(&mut collected, &[0, 1]).await;
            for i in 0..2 {
                assert_eq!(restored[&i], (1, 1), "node {i}: only the surviving peer");
            }
            for i in 0..2 {
                let fake = nodes[i].effect.0.lock().unwrap();
                let other = &nodes[1 - i];
                assert_eq!(fake.applied.len(), 1);
                assert_eq!(fake.applied[0].peers.len(), 1);
                assert_eq!(
                    fake.applied[0].peers[0].allowed_ips,
                    vec![ula(other.identity)]
                );
            }
        })
        .await;
}

/// The parked-resident restart gap: a solo member (the founder shape) whose
/// only WireGuard peer is an admitted-but-parked standby reboots. The
/// standby cannot re-deliver its record — every transport it has rides the
/// overlay the reboot tore down — so the member's restore must reinstall
/// the standby from disk, or the joiner is stranded forever initiating
/// handshakes at a peer that no longer knows its key.
#[tokio::test]
async fn solo_member_cold_restart_reinstalls_the_parked_standby() {
    let dir = tempfile::tempdir().unwrap();

    // life 1: the member applies its solo epoch, then the standby's record
    // pre-warms its tunnel — the accepted record must reach disk (a solo
    // member has no other persist trigger: no peer adverts, no plans).
    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2], vec![]);
                retarget_all(&nodes, &[0], &[1], 1, 10).await;
                spawn_nudgers(&local, &nodes);
                await_prewarmed(&mut collected, &[0], &[(0, 1), (1, 1)], 1).await;
            })
            .await;
    }
    assert!(
        dir.path().join("mesh-0.json").exists(),
        "the member persisted the standby's accepted record"
    );

    // life 2: same directory, ZERO transport — the founder reboot. The
    // member's restore alone must put the standby's key back on the
    // interface (endpoint verbatim from the persisted record: the standby
    // initiates and roams, so the endpoint is a first target, not a
    // requirement).
    let local = LocalSet::new();
    local
        .run_until(async {
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) =
                spawn_mesh_gated(&local, dir.path(), &[1, 2], vec![], links_up.clone());
            retarget_all(&nodes, &[0], &[1], 1, 10).await;

            let restored = await_restored(&mut collected, &[0, 1]).await;
            assert_eq!(restored[&0], (1, 1), "member: the standby from disk");
            assert_eq!(restored[&1], (1, 1), "standby: the member from disk");
            let (standby_keys, _) =
                WireGuardKeypair::load_or_generate(&dir.path().join("wg-1.key")).unwrap();
            let config = latest_config(&nodes[0]);
            let entry = config
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[1].identity)])
                .expect("member: the parked standby is back on the interface");
            assert_eq!(entry.wireguard_public_key, standby_keys.public_key());
            assert_eq!(
                entry.endpoint,
                Some("8.8.8.20:51820".parse().unwrap()),
                "member: the standby's persisted endpoint as first target"
            );

            // links back: the standby's ongoing re-offer supersedes the disk
            // entry live — the full heal the reboot interrupted.
            spawn_nudgers(&local, &nodes);
            links_up.store(true, std::sync::atomic::Ordering::Relaxed);
            await_prewarmed(&mut collected, &[], &[(0, 1)], 1).await;
        })
        .await;
}

/// A boot whose resident set no longer lists a persisted standby restores
/// only the members: the departed standby's tunnel is dead weight and is
/// never applied — the exact gate the member restore applies to adverts.
#[tokio::test]
async fn cold_restart_filters_departed_standbys() {
    let dir = tempfile::tempdir().unwrap();

    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
                retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
                spawn_nudgers(&local, &nodes);
                await_prewarmed(&mut collected, &[0, 1], &[(0, 1), (1, 1), (2, 2)], 1).await;
            })
            .await;
    }

    let local = LocalSet::new();
    local
        .run_until(async {
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) =
                spawn_mesh_gated(&local, dir.path(), &[1, 2, 3], vec![], links_up);
            // the resumed epoch dropped the standby.
            retarget_all(&nodes, &[0, 1], &[], 2, 20).await;

            let restored = await_restored(&mut collected, &[0, 1]).await;
            for i in 0..2 {
                assert_eq!(restored[&i], (1, 1), "node {i}: only the member peer");
                let config = latest_config(&nodes[i]);
                assert_eq!(config.peers.len(), 1, "node {i}: no departed-standby entry");
                assert_eq!(
                    config.peers[0].allowed_ips,
                    vec![ula(nodes[1 - i].identity)]
                );
            }
        })
        .await;
}

/// The NATed-member restart: the coordinated invite bootstrap brings the
/// interface up (the join-window tunnel to the inviter) BEFORE the first
/// epoch event triggers the restore. The restore must land on that live
/// interface — reconfigure, not re-create — and the invite tunnel must
/// survive the merge. Regression: the restore used to die with
/// `AlreadyCreated`, so a restarted member never re-applied its persisted
/// mesh exactly in the posture the restore was built for.
#[tokio::test]
async fn restore_lands_on_an_interface_the_invite_layer_already_created() {
    let dir = tempfile::tempdir().unwrap();

    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2], vec![]);
                retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
                await_applied(&mut collected, &[0, 1], 1).await;
            })
            .await;
    }

    let local = LocalSet::new();
    local
        .run_until(async {
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) =
                spawn_mesh_gated(&local, dir.path(), &[1, 2], vec![], links_up);

            // node 0's boot re-runs first contact with its inviter (an
            // identity OUTSIDE the persisted mesh) before any epoch exists.
            let inviter = PrivateKey::from_seed(9).public_key();
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            nodes[0]
                .cmd
                .send(ReachabilityCommand::InstallInvitePeer {
                    peer: inviter.clone(),
                    wireguard_public_key: X25519PublicKey([9; 32]),
                    endpoint: "203.0.113.77:40100".parse().unwrap(),
                    reply: reachability::InstallReply(reply_tx),
                })
                .await
                .unwrap();
            reply_rx
                .await
                .expect("installer alive")
                .expect("invite peer installed");

            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;

            let restored = await_restored(&mut collected, &[0, 1]).await;
            for i in 0..2 {
                assert_eq!(restored[&i], (1, 1), "node {i}: the persisted peer");
            }
            let fake = nodes[0].effect.0.lock().unwrap();
            assert_eq!(
                fake.create_calls, 1,
                "the invite layer's interface is reconfigured, never re-created"
            );
            assert_eq!(fake.remove_calls, 0);
            assert_eq!(fake.applied.len(), 2, "invite apply, then restore apply");
            let config = &fake.applied[1];
            assert!(
                config
                    .peers
                    .iter()
                    .any(|p| p.allowed_ips == vec![ula(nodes[1].identity)]),
                "the restored mesh peer is on the interface"
            );
            assert!(
                config
                    .peers
                    .iter()
                    .any(|p| p.allowed_ips == vec![ula(binding::identity_of(&inviter))]),
                "the invite tunnel survives the restore"
            );
        })
        .await;
}

/// A tampered state file is refused (surfaced as `RestoreFailed`), never
/// applied — and the node still converges live once transport exists,
/// exactly the pre-persistence behavior.
#[tokio::test]
async fn tampered_mesh_state_is_refused_and_live_assembly_still_converges() {
    let dir = tempfile::tempdir().unwrap();

    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2], vec![]);
                retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
                await_applied(&mut collected, &[0, 1], 1).await;
            })
            .await;
    }
    // flip the epochs inside node 0's file: every owner signature now
    // disowns its record.
    let path = dir.path().join("mesh-0.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("\"epoch\": 1", "\"epoch\": 9")).unwrap();

    let local = LocalSet::new();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2], vec![]);
            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);

            let mut refused = false;
            let mut applied: HashSet<usize> = HashSet::new();
            tokio::time::timeout(CONVERGE_BUDGET, async {
                while !refused || applied.len() < 2 {
                    let (i, event) = collected.recv().await.expect("event stream open");
                    match event {
                        ReachabilityEvent::RestoreFailed { reason } => {
                            assert_eq!(i, 0, "only node 0's file was tampered");
                            assert!(reason.contains("signature"), "{reason}");
                            refused = true;
                        }
                        ReachabilityEvent::TunnelsApplied { epoch: 1, .. } => {
                            applied.insert(i);
                        }
                        ReachabilityEvent::EpochFailed { reason, .. } => {
                            panic!("epoch failed on node {i}: {reason}");
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("refusal surfaced and the mesh still converged");

            // node 0 never applied the tampered mesh: its only apply is the
            // live epoch's.
            let fake = nodes[0].effect.0.lock().unwrap();
            assert_eq!(fake.applied.len(), 1);
        })
        .await;
}

/// Drain collected events until every member in `members` has applied
/// `epoch` AND every `(node, at_least_n_peers)` pair in `standby_want` has
/// been satisfied by a `StandbyTunnelsApplied` for `epoch` — ONE pass over
/// the shared event stream, because a standby apply that rides the member's
/// epoch apply lands in the same drain (pre-warm applies are incremental, so
/// a node may report a partial count before its full set). Failure events
/// during a pre-warm window are bugs.
async fn await_prewarmed(
    collected: &mut mpsc::Receiver<(usize, ReachabilityEvent)>,
    members: &[usize],
    standby_want: &[(usize, usize)],
    epoch: u64,
) {
    let mut applied: HashSet<usize> = HashSet::new();
    let mut latest: HashMap<usize, usize> = HashMap::new();
    tokio::time::timeout(CONVERGE_BUDGET, async {
        while members.iter().any(|i| !applied.contains(i))
            || standby_want
                .iter()
                .any(|(i, n)| latest.get(i).is_none_or(|have| have < n))
        {
            let (i, event) = collected.recv().await.expect("event stream open");
            match event {
                ReachabilityEvent::TunnelsApplied { epoch: e, .. } if e == epoch => {
                    applied.insert(i);
                }
                ReachabilityEvent::StandbyTunnelsApplied {
                    epoch: e, peers, ..
                } if e == epoch => {
                    latest.insert(i, peers);
                }
                ReachabilityEvent::EpochFailed { reason, .. } => {
                    panic!("epoch failed on node {i}: {reason}");
                }
                ReachabilityEvent::PeerFailed { reason, .. } => {
                    panic!("peer failed on node {i}: {reason}");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("pre-warm tunnels applied in time");
}

/// A node's LATEST applied interface config — pre-warm applies reconfigure
/// in place, so the last config is the interface's current truth.
fn latest_config(node: &TestNode) -> InterfaceConfig {
    let fake = node.effect.0.lock().unwrap();
    fake.applied
        .last()
        .expect("node applied at least once")
        .clone()
}

/// The pre-warm headline: a standby's tunnels exist on BOTH sides before its
/// activation. Members merge the standby's record-derived peer onto their
/// live interface without tearing it down; the standby brings up its own
/// interface carrying every member; and the activation cutover then folds it
/// into the verified member mesh.
#[tokio::test]
async fn standby_tunnels_prewarm_before_activation() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            // each member applies the epoch and ends up with the one
            // standby installed; the standby installs both members.
            await_prewarmed(&mut collected, &[0, 1], &[(0, 1), (1, 1), (2, 2)], 1).await;

            let standby = &nodes[2];
            let (standby_keys, _) =
                WireGuardKeypair::load_or_generate(&dir.path().join("wg-2.key")).unwrap();
            for i in [0usize, 1] {
                let config = latest_config(&nodes[i]);
                assert_eq!(config.peers.len(), 2, "member {i}: member peer + standby");
                let entry = config
                    .peers
                    .iter()
                    .find(|p| p.allowed_ips == vec![ula(standby.identity)])
                    .unwrap_or_else(|| panic!("member {i}: no standby peer entry"));
                assert_eq!(entry.wireguard_public_key, standby_keys.public_key());
                assert_eq!(
                    entry.endpoint,
                    Some("8.8.8.30:51820".parse().unwrap()),
                    "member {i}: the standby's advertised endpoint"
                );
                let fake = nodes[i].effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "member {i}: one interface");
                assert_eq!(
                    fake.remove_calls, 0,
                    "member {i}: the pre-warm merge never tears down"
                );
            }
            {
                let config = latest_config(standby);
                assert_eq!(config.addresses, vec![ula(standby.identity)]);
                assert_eq!(config.peers.len(), 2, "standby: both members installed");
                for member in &nodes[..2] {
                    assert!(
                        config
                            .peers
                            .iter()
                            .any(|p| p.allowed_ips == vec![ula(member.identity)]),
                        "standby: missing member peer"
                    );
                }
                let fake = standby.effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "standby: one interface");
                assert_eq!(fake.remove_calls, 0);
            }

            // activation: the standby joins the member set at the next
            // cutover and the verified phase-A mesh replaces the pre-warm
            // layer on every node.
            retarget_all(&nodes, &[0, 1, 2], &[], 2, 20).await;
            let versions = await_applied(&mut collected, &[0, 1, 2], 2).await;
            assert_eq!(versions[&0], versions[&2], "the activated node versions");
            for (i, node) in nodes.iter().enumerate() {
                let config = latest_config(node);
                assert_eq!(config.peers.len(), 2, "node {i}: the full member mesh");
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(
                    fake.remove_calls, 0,
                    "node {i}: the activation cutover reconfigures the pre-warm interface in place"
                );
            }
        })
        .await;
}

/// Install `peer` as `node`'s join-window invite tunnel (the intro/bootstrap
/// path's exact command) and await the applied reply.
async fn install_invite(
    node: &TestNode,
    peer: commonware_cryptography::ed25519::PublicKey,
    wireguard_public_key: X25519PublicKey,
    endpoint: SocketAddr,
) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    node.cmd
        .send(ReachabilityCommand::InstallInvitePeer {
            peer,
            wireguard_public_key,
            endpoint,
            reply: InstallReply(tx),
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();
}

/// The invite-join cutover regression (statesync went dark the moment
/// resident standing landed): an endpoint-less member and an endpoint-less
/// joiner hold a LIVE invite tunnel whose endpoints are OBSERVED (the intro
/// datagram's source on the inviter; the rendezvous-resolved path on the
/// joiner). Standing lands, both sides retarget (member + standby), and the
/// pre-warm records both advertise NO endpoint. The merge must keep the
/// observed invite endpoints: replacing them with the records' `None` leaves
/// BOTH sides unable to initiate — the live tunnel the join rode dies, and
/// with it the joiner's only statesync path.
#[tokio::test]
async fn prewarm_merge_keeps_the_invite_tunnels_observed_endpoints() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            // both nodes endpoint-less: the NATed-desktop default shape.
            let deliver_all: DeliveryFilter = Rc::new(|_, _, _| 1);
            let (nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2],
                vec![],
                deliver_all,
                vec![],
                None,
                &[0, 1],
                &[],
                vec![],
            );
            // pre-create both keystores so each side can install the OTHER
            // as its invite peer; the nodes load these same files at start.
            let (wg0, _) =
                WireGuardKeypair::load_or_generate(&dir.path().join("wg-0.key")).unwrap();
            let (wg1, _) =
                WireGuardKeypair::load_or_generate(&dir.path().join("wg-1.key")).unwrap();
            let observed_joiner: SocketAddr = "203.0.113.9:60579".parse().unwrap();
            let resolved_member: SocketAddr = "203.0.113.8:51820".parse().unwrap();
            install_invite(
                &nodes[0],
                nodes[1].signer.public_key(),
                wg1.public_key(),
                observed_joiner,
            )
            .await;
            install_invite(
                &nodes[1],
                nodes[0].signer.public_key(),
                wg0.public_key(),
                resolved_member,
            )
            .await;

            // the joiner's standing lands: epoch 1 = member 0 + standby 1.
            retarget_all(&nodes, &[0], &[1], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_prewarmed(&mut collected, &[0], &[(0, 1), (1, 1)], 1).await;

            let member_view = latest_config(&nodes[0]);
            let entry = member_view
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[1].identity)])
                .expect("member: joiner peer entry");
            assert_eq!(entry.wireguard_public_key, wg1.public_key());
            assert_eq!(
                entry.endpoint,
                Some(observed_joiner),
                "member: the joiner's observed invite endpoint survives its endpoint-less record"
            );
            let joiner_view = latest_config(&nodes[1]);
            let entry = joiner_view
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[0].identity)])
                .expect("joiner: member peer entry");
            assert_eq!(entry.wireguard_public_key, wg0.public_key());
            assert_eq!(
                entry.endpoint,
                Some(resolved_member),
                "joiner: the member's resolved invite endpoint survives its endpoint-less record"
            );
        })
        .await;
}

/// A standby record that lands while the members' epoch is still assembling
/// rides the epoch's ONE apply instead of forcing an early interface — and
/// the pre-warm layer works through partial links: the standby exchanges
/// gossip with member 0 alone while member 1 is still down.
#[tokio::test]
async fn standby_record_before_the_epoch_apply_rides_it() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            let member_pks: Vec<_> = [0usize, 1]
                .iter()
                .map(|i| nodes[*i].signer.public_key())
                .collect();
            let standby_pks = vec![nodes[2].signer.public_key()];
            // member 0 and the standby retarget; member 1 stays dark, so
            // member 0's epoch CANNOT apply yet.
            for i in [0usize, 2] {
                nodes[i]
                    .cmd
                    .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                        epoch: 1,
                        members: member_pks.clone(),
                        standbys: standby_pks.clone(),
                        current_view: 10,
                    }))
                    .await
                    .unwrap();
            }
            spawn_nudgers(&local, &nodes);
            // the standby installs member 0 (proof its record reached member
            // 0 and the first-contact reply flowed back) while member 0
            // holds the record for the epoch apply.
            await_prewarmed(&mut collected, &[], &[(2, 1)], 1).await;
            {
                let fake = nodes[0].effect.0.lock().unwrap();
                assert!(
                    fake.applied.is_empty(),
                    "member 0 holds the pre-warm peer until its epoch applies"
                );
            }

            // member 1 arrives; the epoch completes; member 0's FIRST apply
            // already carries the standby peer.
            nodes[1]
                .cmd
                .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                    epoch: 1,
                    members: member_pks.clone(),
                    standbys: standby_pks.clone(),
                    current_view: 10,
                }))
                .await
                .unwrap();
            await_prewarmed(&mut collected, &[0, 1], &[(1, 1), (2, 2)], 1).await;
            let standby = &nodes[2];
            {
                let fake = nodes[0].effect.0.lock().unwrap();
                let first = &fake.applied[0];
                assert_eq!(
                    first.peers.len(),
                    2,
                    "member 0's first apply carries member 1 AND the standby"
                );
                assert!(
                    first
                        .peers
                        .iter()
                        .any(|p| p.allowed_ips == vec![ula(standby.identity)]),
                    "the standby rode the epoch apply"
                );
            }
        })
        .await;
}

/// Live re-advertisement: a standby's higher-nonce record moves its tunnel
/// endpoint in place on every member, and a stale lower-nonce replay is
/// silently ignored.
#[tokio::test]
async fn standby_readvertisement_updates_the_endpoint_live() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_prewarmed(&mut collected, &[0, 1], &[(0, 1), (1, 1)], 1).await;

            // the standby re-advertises from a new address (a NAT rebind):
            // same identity and WireGuard key, higher nonce, new endpoint.
            let policy = PortPolicy::production();
            let set =
                reachability::active_set(CHAIN, 1, vec![nodes[0].identity, nodes[1].identity])
                    .unwrap();
            let (standby_keys, _) =
                WireGuardKeypair::load_or_generate(&dir.path().join("wg-2.key")).unwrap();
            let rebind = |nonce: u64, octet: u8| {
                wireguard::SignedEndpointRecord::sign(
                    wireguard::EndpointRecord {
                        namespace: CHAIN.into(),
                        epoch: 1,
                        valset_root: set.valset_root,
                        admission_root: set.admission_root,
                        validator_identity: nodes[2].identity,
                        wireguard_public_key: standby_keys.public_key(),
                        control_endpoint: endpoint(&policy, 30, 443, Transport::Tcp),
                        wireguard_endpoint: Some(
                            Endpoint::new(
                                std::net::IpAddr::V4(std::net::Ipv4Addr::new(9, 9, 9, octet)),
                                51820,
                                Transport::Udp,
                                &policy,
                            )
                            .unwrap(),
                        ),
                        nonce,
                    },
                    &nodes[2].signer,
                )
            };
            // the live record's nonce is wall-clock-seeded (issue #1102), so
            // "higher" means "later than the standby signed at retarget" — a
            // rebind minutes in the future — and "stale" is anything at or
            // below the seed's floor.
            let later = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0)
                + 60_000;
            nodes[0]
                .cmd
                .send(ReachabilityCommand::Deliver {
                    from: nodes[2].signer.public_key(),
                    bytes: ReachabilityMsg::Record(rebind(later, 42)).encode(),
                })
                .await
                .unwrap();
            // both members converge on the new endpoint — member 1 through
            // member 0's accept-gated relay of the fresher record.
            let moved: std::net::SocketAddr = "9.9.9.42:51820".parse().unwrap();
            tokio::time::timeout(CONVERGE_BUDGET, async {
                loop {
                    let both = [0usize, 1].iter().all(|i| {
                        latest_config(&nodes[*i])
                            .peers
                            .iter()
                            .any(|p| p.endpoint == Some(moved))
                    });
                    if both {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("both members moved the standby's endpoint");

            // a stale replay (lower nonce, yet another address) changes
            // nothing: deliver it, then prove the endpoint still stands.
            nodes[0]
                .cmd
                .send(ReachabilityCommand::Deliver {
                    from: nodes[2].signer.public_key(),
                    bytes: ReachabilityMsg::Record(rebind(1, 77)).encode(),
                })
                .await
                .unwrap();
            // drive a full nudge round so the stale record would have had
            // every chance to mis-apply.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let config = latest_config(&nodes[0]);
            let entry = config
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[2].identity)])
                .expect("standby entry present");
            assert_eq!(entry.endpoint, Some(moved), "the stale replay was ignored");
        })
        .await;
}

/// Issue #1102 — the REBOOT form of the re-advertisement above: a standby
/// that restarts mid-epoch re-signs its record for the SAME epoch tuple in a
/// fresh plane life, so whatever nonce `retarget` REALLY signs is
/// what the members judge (no hand-minted nonce here). The previous life's
/// nonce is already burnt into every member's pre-warm gate; a fixed nonce
/// seed replay-drops the reboot's re-introduction for the rest of the epoch
/// and the members keep dialing the dead pre-reboot address. The record
/// nonce is wall-clock-seeded precisely so the reboot supersedes in place.
#[tokio::test]
async fn standby_reboot_supersedes_its_previous_life_in_place() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_prewarmed(&mut collected, &[0, 1], &[(0, 1), (1, 1), (2, 2)], 1).await;

            // both members hold the standby at its life-1 address.
            let life1: SocketAddr = "8.8.8.30:51820".parse().unwrap();
            for i in [0usize, 1] {
                assert!(
                    latest_config(&nodes[i])
                        .peers
                        .iter()
                        .any(|p| p.endpoint == Some(life1)),
                    "member {i}: the life-1 standby endpoint installed"
                );
            }

            // the standby's process dies. an orderly Shutdown stands in for
            // the crash — its run() exits, its interface with it — while the
            // members keep their in-memory epoch state untouched, which is
            // the point: nothing re-targeted THERE, so their dedup gates
            // still carry the previous life's nonce.
            nodes[2]
                .cmd
                .send(ReachabilityCommand::Shutdown)
                .await
                .unwrap();

            // life 2: the same identity, keystore, and persist file, back
            // behind a DIFFERENT address (the NAT-remap restart shape).
            let reborn = respawn_node(
                &local,
                dir.path(),
                &nodes[2],
                40,
                false,
                StaticResolver::default(),
            );
            let members: Vec<_> = [0usize, 1]
                .iter()
                .map(|i| nodes[*i].signer.public_key())
                .collect();
            reborn
                .cmd
                .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                    epoch: 1,
                    members,
                    standbys: vec![reborn.signer.public_key()],
                    current_view: 12,
                }))
                .await
                .unwrap();
            // the reborn nudger: re-offering its record is the standby's
            // whole job.
            spawn_nudgers(&local, std::slice::from_ref(&reborn));

            // both members adopt the reboot's address IN PLACE — the
            // re-signed record supersedes the previous life's.
            let life2: SocketAddr = "8.8.8.40:51820".parse().unwrap();
            tokio::time::timeout(CONVERGE_BUDGET, async {
                loop {
                    let both = [0usize, 1].iter().all(|i| {
                        latest_config(&nodes[*i])
                            .peers
                            .iter()
                            .any(|p| p.endpoint == Some(life2))
                    });
                    if both {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("members adopted the reboot's re-advertised address");
        })
        .await;
}

/// A MEMBER that restarts mid-epoch re-converges IN PLACE, on both sides.
/// The members judge the reboot's fresh record against the nonce they
/// locked and re-point its tunnel live — never a teardown, never a failed
/// epoch — while the reborn node, whose own fresh record can never hash to
/// the version its peers locked, ADOPTS their unanimous version and
/// installs their records as its applied base. The next cutover folds
/// everything back into one verified mesh.
#[tokio::test]
async fn member_reboot_readvertises_and_adopts_the_locked_mesh_in_place() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (mut nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_applied(&mut collected, &[0, 1, 2], 1).await;

            // node 2's process dies mid-epoch; the members' state is
            // untouched — nothing retargets, their locked mesh stands.
            nodes[2]
                .cmd
                .send(ReachabilityCommand::Shutdown)
                .await
                .unwrap();

            // life 2: same identity, keystore, and persist file, back
            // behind a different address, mid-epoch.
            let reborn = respawn_node(
                &local,
                dir.path(),
                &nodes[2],
                40,
                false,
                StaticResolver::default(),
            );
            nodes[2] = reborn;
            let members: Vec<_> = nodes.iter().map(|n| n.signer.public_key()).collect();
            nodes[2]
                .cmd
                .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                    epoch: 1,
                    members,
                    standbys: vec![],
                    current_view: 12,
                }))
                .await
                .unwrap();
            spawn_nudgers(&local, std::slice::from_ref(&nodes[2]));

            // both members re-point the reboot's tunnel; the reborn node
            // adopts the peers' locked mesh and applies it.
            let mut repointed = HashSet::new();
            let mut adopted = false;
            let mut reborn_applied = false;
            tokio::time::timeout(CONVERGE_BUDGET, async {
                while !(repointed.len() == 2 && adopted && reborn_applied) {
                    let (i, event) = collected.recv().await.expect("event stream open");
                    match event {
                        ReachabilityEvent::PeerReadvertised { .. } => {
                            repointed.insert(i);
                        }
                        ReachabilityEvent::MeshAdopted { epoch: 1, .. } if i == 2 => {
                            adopted = true;
                        }
                        ReachabilityEvent::TunnelsApplied { epoch: 1, .. } if i == 2 => {
                            reborn_applied = true;
                        }
                        ReachabilityEvent::EpochFailed { reason, .. } => {
                            panic!("epoch failed on node {i}: {reason}");
                        }
                        ReachabilityEvent::PeerFailed { reason, .. } => {
                            panic!("peer failed on node {i}: {reason}");
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("the mid-epoch reboot re-converged in place");

            let life2: SocketAddr = "8.8.8.40:51820".parse().unwrap();
            let (reborn_keys, _) =
                WireGuardKeypair::load_or_generate(&dir.path().join("wg-2.key")).unwrap();
            for i in [0usize, 1] {
                let config = latest_config(&nodes[i]);
                let entry = config
                    .peers
                    .iter()
                    .find(|p| p.allowed_ips == vec![ula(nodes[2].identity)])
                    .unwrap_or_else(|| panic!("member {i}: no entry for the reborn member"));
                assert_eq!(
                    entry.endpoint,
                    Some(life2),
                    "member {i}: the reboot's address"
                );
                assert_eq!(entry.wireguard_public_key, reborn_keys.public_key());
                let fake = nodes[i].effect.0.lock().unwrap();
                assert_eq!(fake.remove_calls, 0, "member {i}: never a teardown");
                assert_eq!(
                    fake.applied.len(),
                    2,
                    "member {i}: the epoch apply, then the one re-point"
                );
            }
            {
                let config = latest_config(&nodes[2]);
                assert_eq!(config.peers.len(), 2, "reborn: both members installed");
                for member in &nodes[..2] {
                    let entry = config
                        .peers
                        .iter()
                        .find(|p| p.allowed_ips == vec![ula(member.identity)])
                        .unwrap_or_else(|| panic!("reborn: missing a member peer"));
                    let expected: SocketAddr =
                        format!("8.8.8.{}:51820", member.octet).parse().unwrap();
                    assert_eq!(entry.endpoint, Some(expected));
                }
                let fake = nodes[2].effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "reborn: one interface");
                assert_eq!(fake.remove_calls, 0);
                assert_eq!(
                    fake.applied.len(),
                    2,
                    "reborn: the cold restore, then the adoption"
                );
            }

            // the next cutover folds the fresh record back into a verified
            // mesh on every node.
            retarget_all(&nodes, &[0, 1, 2], &[], 2, 20).await;
            let versions = await_applied(&mut collected, &[0, 1, 2], 2).await;
            assert_eq!(versions[&0], versions[&2], "one verified version again");
        })
        .await;
}

/// The same reboot with the node's persisted mesh GONE (a lost state
/// directory): nothing restores, so adopting the peers' lock is the only
/// thing that can bring the interface up at all — and the peers' records
/// include an endpoint-less member, which the adoption installs
/// endpoint-less and the post-apply rendezvous sweep then heals on the
/// LIVE interface (the resolved override written through in place).
#[tokio::test]
async fn member_reboot_without_its_mesh_file_adopts_the_peers_lock() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            // node 1 advertises no endpoint (the NATed member shape); the
            // in-memory router still carries its handshakes, so epoch 1
            // converges with node 1 installed endpoint-less everywhere.
            let (mut nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2, 3],
                vec![],
                Rc::new(|_, _, _| 1),
                vec![],
                None,
                &[1],
                &[],
                vec![],
            );
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_applied(&mut collected, &[0, 1, 2], 1).await;

            nodes[2]
                .cmd
                .send(ReachabilityCommand::Shutdown)
                .await
                .unwrap();
            // the crash also lost the node's state directory.
            std::fs::remove_file(dir.path().join("mesh-2.json")).unwrap();

            // life 2 has a coordinator and a resolver that can punch the
            // endpoint-less member by identity.
            let punched: SocketAddr = "203.0.113.88:42042".parse().unwrap();
            let mut rendezvous = StaticResolver::default();
            rendezvous.0.insert(
                binding::node_key(nodes[1].identity),
                Resolution::Punched(punched),
            );
            let reborn = respawn_node(&local, dir.path(), &nodes[2], 40, true, rendezvous);
            nodes[2] = reborn;
            let members: Vec<_> = nodes.iter().map(|n| n.signer.public_key()).collect();
            nodes[2]
                .cmd
                .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                    epoch: 1,
                    members,
                    standbys: vec![],
                    current_view: 12,
                }))
                .await
                .unwrap();
            spawn_nudgers(&local, std::slice::from_ref(&nodes[2]));

            let mut adopted = false;
            let mut healed = false;
            tokio::time::timeout(CONVERGE_BUDGET, async {
                while !(adopted && healed) {
                    let (i, event) = collected.recv().await.expect("event stream open");
                    match event {
                        ReachabilityEvent::MeshAdopted { epoch: 1, .. } if i == 2 => {
                            adopted = true;
                        }
                        ReachabilityEvent::PeerEndpointResolved { endpoint, .. } if i == 2 => {
                            assert_eq!(endpoint, punched, "the rigged punch is what resolved");
                            healed = true;
                        }
                        ReachabilityEvent::EpochFailed { reason, .. } => {
                            panic!("epoch failed on node {i}: {reason}");
                        }
                        ReachabilityEvent::PeerFailed { reason, .. } => {
                            panic!("peer failed on node {i}: {reason}");
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("adoption from nothing, then the live endpoint heal");

            let fake = nodes[2].effect.0.lock().unwrap();
            assert_eq!(
                fake.create_calls, 1,
                "no mesh file: the adoption is what brought the interface up"
            );
            assert_eq!(fake.remove_calls, 0);
            let config = fake.applied.last().expect("the sweep re-applied");
            let advertised = config
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[0].identity)])
                .expect("the advertised member installed");
            assert_eq!(advertised.endpoint, Some("8.8.8.10:51820".parse().unwrap()));
            let endpoint_less = config
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[1].identity)])
                .expect("the endpoint-less member installed");
            assert_eq!(
                endpoint_less.endpoint,
                Some(punched),
                "the post-apply resolution reconfigured the live interface"
            );
        })
        .await;
}

/// A record from an identity in NEITHER class — however well-signed — is
/// refused loudly and installs nothing.
#[tokio::test]
async fn record_from_neither_member_nor_standby_is_refused() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2], vec![]);
            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            await_applied(&mut collected, &[0, 1], 1).await;

            let stranger = PrivateKey::from_seed(99);
            let policy = PortPolicy::production();
            let set =
                reachability::active_set(CHAIN, 1, vec![nodes[0].identity, nodes[1].identity])
                    .unwrap();
            let forged = wireguard::SignedEndpointRecord::sign(
                wireguard::EndpointRecord {
                    namespace: CHAIN.into(),
                    epoch: 1,
                    valset_root: set.valset_root,
                    admission_root: set.admission_root,
                    validator_identity: binding::identity_of(&stranger.public_key()),
                    wireguard_public_key: wireguard::X25519PublicKey([9; 32]),
                    control_endpoint: endpoint(&policy, 99, 443, Transport::Tcp),
                    wireguard_endpoint: Some(endpoint(&policy, 99, 51820, Transport::Udp)),
                    nonce: 1,
                },
                &stranger,
            );
            // relayed by a MEMBER, so the via-gate passes and the identity
            // check itself must refuse it.
            nodes[0]
                .cmd
                .send(ReachabilityCommand::Deliver {
                    from: nodes[1].signer.public_key(),
                    bytes: ReachabilityMsg::Record(forged).encode(),
                })
                .await
                .unwrap();
            let reason = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some((0, ReachabilityEvent::PeerFailed { reason, .. })) =
                        collected.recv().await
                    {
                        break reason;
                    }
                }
            })
            .await
            .expect("the stranger record surfaced");
            assert!(reason.contains("unknown identity"), "{reason}");
            let fake = nodes[0].effect.0.lock().unwrap();
            assert_eq!(
                fake.applied.len(),
                1,
                "nothing beyond the member mesh applied"
            );
        })
        .await;
}

/// The lobby shape: the standby's transport identity is the network's shared
/// ingress key, not its record identity. Members must admit its gossip via
/// `gossip_ingress`, learn the route, and address every standby-directed
/// reply to the ingress identity — full pre-warm convergence on both sides.
#[tokio::test]
async fn standby_gossip_rides_the_lobby_ingress_identity() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let lobby = PrivateKey::from_seed(77).public_key();
            let deliver_all: DeliveryFilter = Rc::new(|_, _, _| 1);
            let (nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2, 3],
                vec![],
                deliver_all,
                vec![None, None, Some(lobby.clone())],
                Some(lobby),
                &[],
                &[],
                vec![],
            );
            retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_prewarmed(&mut collected, &[0, 1], &[(0, 1), (1, 1), (2, 2)], 1).await;

            let standby = &nodes[2];
            for i in [0usize, 1] {
                let config = latest_config(&nodes[i]);
                assert!(
                    config
                        .peers
                        .iter()
                        .any(|p| p.allowed_ips == vec![ula(standby.identity)]),
                    "member {i}: standby tunnel installed via the lobby ingress"
                );
            }
            assert_eq!(latest_config(standby).peers.len(), 2);
        })
        .await;
}

/// The promotion-reboot payoff: a standby persists the member adverts it
/// collected while pre-warming, and its next process life — booting as a
/// MEMBER of the widened epoch with ZERO transport — restores every member
/// tunnel from disk alone. A mid-standby cold restart restores the same way.
#[tokio::test]
async fn standby_persists_the_member_mesh_for_its_promotion_reboot() {
    let dir = tempfile::tempdir().unwrap();

    // life 1: pre-warm as a standby; the member adverts land on disk.
    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
                retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
                spawn_nudgers(&local, &nodes);
                await_prewarmed(&mut collected, &[0, 1], &[(2, 2)], 1).await;
                // the standby persists on advert acceptance; both member
                // adverts must be on disk before this life ends.
                tokio::time::timeout(CONVERGE_BUDGET, async {
                    loop {
                        let full =
                            reachability::store::load(&dir.path().join("mesh-2.json"), CHAIN)
                                .ok()
                                .flatten()
                                .is_some_and(|mesh| mesh.adverts.len() == 2);
                        if full {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("the standby persisted both member adverts");
            })
            .await;
    }

    // life 2: a mid-standby cold restart — same standby role, no transport.
    {
        let local = LocalSet::new();
        local
            .run_until(async {
                let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (nodes, mut collected) =
                    spawn_mesh_gated(&local, dir.path(), &[1, 2, 3], vec![], links_up);
                retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
                let restored = await_restored(&mut collected, &[2]).await;
                assert_eq!(restored[&2], (1, 2), "both member tunnels from disk");
            })
            .await;
    }

    // life 3: the promotion reboot — the standby boots as a MEMBER of the
    // widened epoch, still with no transport, and the pre-warm era's
    // persisted mesh carries its first gossip.
    let local = LocalSet::new();
    local
        .run_until(async {
            let links_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (nodes, mut collected) =
                spawn_mesh_gated(&local, dir.path(), &[1, 2, 3], vec![], links_up);
            retarget_all(&nodes, &[0, 1, 2], &[], 2, 20).await;
            let restored = await_restored(&mut collected, &[2]).await;
            assert_eq!(
                restored[&2],
                (1, 2),
                "the promoted node restored both member tunnels from its standby era"
            );
            let config = latest_config(&nodes[2]);
            assert_eq!(config.peers.len(), 2);
            for member in &nodes[..2] {
                assert!(
                    config
                        .peers
                        .iter()
                        .any(|p| p.allowed_ips == vec![ula(member.identity)]),
                    "restored peer for each member"
                );
            }
        })
        .await;
}

/// A [`StaticResolver`] that ALSO answers the coordinated invite's intro
/// datagram: `resolve` returns the punched underlay endpoint from the fixed
/// map (so `resolve_rendezvous_endpoint` succeeds), and `send_datagram_and_recv`
/// hands back a canned ack — the inviter's `IntroAck` riding home over the same
/// punched socket. Lets a plane test drive
/// [`ReachabilityCommand::BootstrapCoordinatedInvitePeer`] end to end
/// (resolve -> install -> ack) with no real UDP.
/// what the resolver's `send_datagram_and_recv` observed — `(dest, intro
/// bytes)`, shared back to the test that drove the bootstrap.
type SentIntro = Rc<RefCell<Option<(SocketAddr, Vec<u8>)>>>;

struct CoordinatedAckResolver {
    inner: StaticResolver,
    ack: Vec<u8>,
    sent: SentIntro,
}

impl reachability::EndpointResolver for CoordinatedAckResolver {
    async fn resolve(
        &mut self,
        peer: NodeKey,
        advertised: SocketAddr,
    ) -> Result<Resolution, String> {
        self.inner.resolve(peer, advertised).await
    }

    async fn send_datagram_and_recv(
        &mut self,
        peer: SocketAddr,
        bytes: Vec<u8>,
        _timeout: Duration,
    ) -> Result<Vec<u8>, String> {
        *self.sent.borrow_mut() = Some((peer, bytes));
        Ok(self.ack.clone())
    }
}

/// `BootstrapCoordinatedInvitePeer` over a [`StaticResolver`]: the inviter's
/// node-key resolves to a punched underlay endpoint, the plane installs
/// it as a join-window tunnel peer, sends the intro over that same socket, and
/// the reply carries the inviter's ack back. Proves the whole
/// resolve -> install -> ack path #260 built, with no real WireGuard or UDP.
#[tokio::test(flavor = "current_thread")]
async fn bootstrap_coordinated_invite_resolves_installs_and_acks() {
    let dir = tempfile::tempdir().expect("plane tempdir");
    let local = LocalSet::new();
    local
        .run_until(async {
            let me = PrivateKey::from_seed(1);
            let inviter = PrivateKey::from_seed(2);
            let inviter_pk = inviter.public_key();

            // the resolver punches the inviter's node-key to a concrete underlay
            // endpoint and answers its intro with a canned ack.
            let punched: SocketAddr = "203.0.113.7:51820".parse().unwrap();
            let mut map = HashMap::new();
            map.insert(
                binding::node_key(binding::identity_of(&inviter_pk)),
                Resolution::Punched(punched),
            );
            let ack = b"coordinated-intro-ack".to_vec();
            let sent = Rc::new(RefCell::new(None));
            let resolver = CoordinatedAckResolver {
                inner: StaticResolver(map),
                ack: ack.clone(),
                sent: sent.clone(),
            };

            let policy = PortPolicy::production();
            let effect = SharedFake::default();
            let config = ReachabilityConfig {
                chain_id: CHAIN.into(),
                signer: me.clone(),
                wireguard_key_file: dir.path().join("wg-me.key"),
                wireguard_port: 51820,
                wireguard_advertised: Some(endpoint(&policy, 10, 51820, Transport::Udp)),
                control_endpoint: endpoint(&policy, 10, 443, Transport::Tcp),
                coordinators: vec![],
                port_policy: policy.clone(),
                persist_file: None,
                gossip_ingress: None,
                backend: NetstackBackend::Native,
            };
            let (cmd_tx, cmd_rx) = mpsc::channel(8);
            let (ev_tx, mut ev_rx) = mpsc::channel(64);
            tokio::task::spawn_local(reachability::run(
                config,
                effect.clone(),
                resolver,
                cmd_rx,
                ev_tx,
            ));

            // drive the coordinated bootstrap.
            let inviter_wg = X25519PublicKey([9u8; 32]);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            cmd_tx
                .send(ReachabilityCommand::BootstrapCoordinatedInvitePeer {
                    peer: inviter_pk.clone(),
                    wireguard_public_key: inviter_wg,
                    intro: b"intro-request".to_vec(),
                    reply: reachability::CoordinatedInviteReply(reply_tx),
                })
                .await
                .expect("bootstrap command accepted");

            // the reply carries the ack the resolver sent back over the punched
            // underlay — resolve -> install -> ack completed.
            let got = tokio::time::timeout(Duration::from_secs(5), reply_rx)
                .await
                .expect("coordinated bootstrap replied in time")
                .expect("reply channel intact")
                .expect("coordinated bootstrap succeeded");
            assert_eq!(
                got, ack,
                "the bootstrap returns the inviter's IntroAck bytes"
            );

            // the intro went to the RESOLVED punched endpoint, not the advertised
            // one — the coordinated path used rendezvous, not a baked address.
            let (dest, intro) = sent.borrow().clone().expect("resolver saw the intro send");
            assert_eq!(
                dest, punched,
                "intro sent to the coordinator-punched endpoint"
            );
            assert_eq!(
                intro, b"intro-request",
                "the joiner's own intro rode across"
            );

            // the inviter is now a join-window peer on the interface.
            let installed = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match ev_rx.recv().await.expect("event stream open") {
                        ReachabilityEvent::InvitePeerInstalled { peer, .. } => break peer,
                        _ => continue,
                    }
                }
            })
            .await
            .expect("InvitePeerInstalled emitted");
            assert_eq!(installed, inviter_pk, "the inviter identity was installed");

            // inspect the applied interface in a tight scope so the mutex guard
            // is released before the shutdown await below.
            {
                let fake = effect.0.lock().unwrap();
                let last = fake
                    .applied
                    .last()
                    .expect("the coordinated install applied an interface config");
                assert!(
                    last.peers
                        .iter()
                        .any(|p| p.wireguard_public_key.0 == [9u8; 32]),
                    "the inviter's wireguard key is a peer on the applied interface"
                );
            }

            cmd_tx
                .send(ReachabilityCommand::Shutdown)
                .await
                .expect("shutdown accepted");
        })
        .await;
}

// ============================================================================
// change 2 / issue #331: coordinated rendezvous fallback for endpoint-less
// admitted members. Two members who each advertise NO WireGuard endpoint (the
// product default for every invite-joined node — `wireguard_advertised:
// None`) can never initiate a handshake on their own; WITH a coordinator
// configured, `resolve_peer` now falls back to by-identity rendezvous
// (`resolve_rendezvous_endpoint`, the same machinery `BootstrapCoordinated-
// InvitePeer` already uses) instead of installing the peer endpoint-less and
// waiting forever.
// ============================================================================

/// RED-first proof of the fallback: two endpoint-less members, each with a
/// coordinator configured, whose `StaticResolver`s are rigged to punch each
/// other. Before change 2, `resolve_peer` returned immediately for an
/// endpoint-less record (ORCH:2251-2256, pre-change) and the resolver was
/// NEVER consulted — both sides would apply with `peer.endpoint == None`.
/// After change 2, the fallback resolves and installs the punched addr.
#[tokio::test]
async fn endpoint_less_members_resolve_via_coordinator_rendezvous_fallback() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let identity_of_seed =
                |seed: u64| binding::identity_of(&PrivateKey::from_seed(seed).public_key());
            let punched_by_0: SocketAddr = "198.51.100.10:41001".parse().unwrap();
            let punched_by_1: SocketAddr = "198.51.100.20:41002".parse().unwrap();

            let mut r0 = StaticResolver::default();
            r0.0.insert(
                binding::node_key(identity_of_seed(2)),
                Resolution::Punched(punched_by_0),
            );
            let mut r1 = StaticResolver::default();
            r1.0.insert(
                binding::node_key(identity_of_seed(1)),
                Resolution::Punched(punched_by_1),
            );

            let (nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2],
                vec![r0, r1],
                Rc::new(|_, _, _| 1),
                vec![],
                None,
                // both nodes endpoint-less...
                &[0, 1],
                // ...and both have a coordinator configured.
                &[0, 1],
                vec![],
            );

            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            // await_applied panics on any PeerFailed — a healthy fallback
            // resolve never emits one.
            await_applied(&mut collected, &[0, 1], 1).await;

            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                let config = fake.applied.last().expect("node applied");
                let peer = config.peers.first().expect("the one peer entry");
                let expected = if i == 0 { punched_by_0 } else { punched_by_1 };
                assert_eq!(
                    peer.endpoint,
                    Some(expected),
                    "node {i}: the fallback-resolved punched endpoint must be installed, not None"
                );
            }
        })
        .await;
}

/// Regression / "no resolver -> today's behavior": two endpoint-less members
/// with NO coordinator configured. The fallback must never even touch the
/// resolver (both `StaticResolver`s are empty maps — any query would default
/// to `Resolution::Advertised`, which `resolve_rendezvous_endpoint` turns
/// into an `Err`, which would emit a `PeerFailed` and fail this test via
/// `await_applied`'s panic). Both peers stay installed endpoint-less, exactly
/// like before change 2 — the peer's own initiation is still what completes
/// the tunnel.
#[tokio::test]
async fn endpoint_less_members_without_a_coordinator_stay_endpoint_less() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2],
                vec![StaticResolver::default(), StaticResolver::default()],
                Rc::new(|_, _, _| 1),
                vec![],
                None,
                &[0, 1], // endpoint-less...
                &[],     // ...but no coordinator configured for either.
                vec![],
            );

            retarget_all(&nodes, &[0, 1], &[], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_applied(&mut collected, &[0, 1], 1).await;

            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                let config = fake.applied.last().expect("node applied");
                let peer = config.peers.first().expect("the one peer entry");
                assert_eq!(
                    peer.endpoint, None,
                    "node {i}: no coordinator configured — must stay endpoint-less exactly like \
                     before change 2"
                );
            }
        })
        .await;
}

/// Issue #1104 — the reboot deadlock's second half: a fully-NATed standby
/// restarts, `restore()` reinstalls its members from the persisted mesh —
/// but an endpoint-less member reinstalls endpoint-less, and with both
/// sides NATed neither can initiate. With a coordinator configured, the
/// reborn standby's nudge sweep must rendezvous such members BY IDENTITY
/// and graft the punched address onto the pre-warm layer, instead of
/// sitting dark for the life of the process.
#[tokio::test]
async fn standby_reboot_rendezvouses_its_endpointless_members() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            // members 0 (endpoint-less, the NATed shape) and 1 (advertised),
            // standby 2. Life 1 configures no coordinators anywhere: the
            // standby installs member 0 endpoint-less — today's behavior —
            // and persists the advert set the reboot restores from.
            let (nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2, 3],
                vec![],
                Rc::new(|_, _, _| 1),
                vec![],
                None,
                &[0],
                &[],
                vec![],
            );
            retarget_all(&nodes, &[0, 1], &[2], 1, 10).await;
            spawn_nudgers(&local, &nodes);
            await_prewarmed(&mut collected, &[0, 1], &[(2, 2)], 1).await;
            // both member ADVERTS must reach the standby's persist file
            // before the crash — that file is everything life 2 has.
            let persist = dir.path().join("mesh-2.json");
            tokio::time::timeout(CONVERGE_BUDGET, async {
                loop {
                    let persisted = reachability::store::load(&persist, CHAIN).ok().flatten();
                    if persisted.is_some_and(|mesh| mesh.adverts.len() == 2) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the standby persisted both member adverts");

            // the standby's process dies; the members stay up untouched.
            nodes[2]
                .cmd
                .send(ReachabilityCommand::Shutdown)
                .await
                .unwrap();

            // life 2: the same identity, keystore, and persist file — now
            // WITH a coordinator, and a resolver rigged to punch member 0.
            // No router wiring back to the members: plane gossip rides the
            // very tunnels being healed, so nothing can arrive.
            let punched: SocketAddr = "203.0.113.77:41414".parse().unwrap();
            let mut rendezvous = StaticResolver::default();
            rendezvous.0.insert(
                binding::node_key(nodes[0].identity),
                Resolution::Punched(punched),
            );
            let policy = PortPolicy::production();
            let reborn_effect = SharedFake::default();
            let (reborn_cmd, reborn_cmd_rx) = mpsc::channel(256);
            let (reborn_ev_tx, mut reborn_ev_rx) = mpsc::channel(256);
            let config = ReachabilityConfig {
                chain_id: CHAIN.into(),
                signer: nodes[2].signer.clone(),
                wireguard_key_file: dir.path().join("wg-2.key"),
                wireguard_port: 51820,
                // fully NATed: the reborn resident advertises nothing.
                wireguard_advertised: None,
                control_endpoint: endpoint(&policy, 30, 443, Transport::Tcp),
                coordinators: vec!["203.0.113.53:3478".parse().unwrap()],
                port_policy: policy.clone(),
                persist_file: Some(persist),
                gossip_ingress: None,
                backend: NetstackBackend::Native,
            };
            local.spawn_local(reachability::run(
                config,
                reborn_effect.clone(),
                rendezvous,
                reborn_cmd_rx,
                reborn_ev_tx,
            ));
            // drain the reborn side's events — its record re-offers have
            // nowhere to land, and the assertions read the effect directly.
            local.spawn_local(async move { while reborn_ev_rx.recv().await.is_some() {} });
            let members: Vec<_> = [0usize, 1]
                .iter()
                .map(|i| nodes[*i].signer.public_key())
                .collect();
            reborn_cmd
                .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                    epoch: 1,
                    members,
                    standbys: vec![nodes[2].signer.public_key()],
                    current_view: 12,
                }))
                .await
                .unwrap();
            {
                let cmd = reborn_cmd.clone();
                local.spawn_local(async move {
                    let mut tick = tokio::time::interval(Duration::from_millis(50));
                    loop {
                        tick.tick().await;
                        if cmd.send(ReachabilityCommand::Nudge).await.is_err() {
                            break;
                        }
                    }
                });
            }

            // the sweep rendezvouses member 0 and re-applies: its entry now
            // carries the punched address instead of sitting endpoint-less.
            tokio::time::timeout(CONVERGE_BUDGET, async {
                loop {
                    let healed = {
                        let fake = reborn_effect.0.lock().unwrap();
                        fake.applied.last().is_some_and(|config| {
                            config.peers.iter().any(|p| {
                                p.allowed_ips == vec![ula(nodes[0].identity)]
                                    && p.endpoint == Some(punched)
                            })
                        })
                    };
                    if healed {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the reborn standby rendezvoused its endpoint-less member");
            // the advertised member restored untouched, on its own address.
            let advertised: SocketAddr = "8.8.8.20:51820".parse().unwrap();
            let config = {
                let fake = reborn_effect.0.lock().unwrap();
                fake.applied.last().unwrap().clone()
            };
            let member1 = config
                .peers
                .iter()
                .find(|p| p.allowed_ips == vec![ula(nodes[1].identity)])
                .expect("member 1 restored");
            assert_eq!(member1.endpoint, Some(advertised));
        })
        .await;
}

/// A node whose machine is the wasm guest converges with native peers onto
/// the same mesh: past the executor the backend is invisible.
#[tokio::test]
async fn a_guest_backed_node_converges_with_native_peers() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh_transported(
                &local,
                dir.path(),
                &[1, 2, 3],
                vec![],
                Rc::new(|_, _, _| 1),
                vec![],
                None,
                &[],
                &[],
                vec![guest_backend(reachability::NETSTACK_STEP_FUEL)],
            );
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            let versions = await_applied(&mut collected, &[0, 1, 2], 1).await;

            assert_eq!(versions.len(), 3);
            assert_eq!(versions[&0], versions[&1]);
            assert_eq!(versions[&0], versions[&2]);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "node {i}: one interface");
                assert_eq!(fake.applied.len(), 1, "node {i}: one apply");
                assert_eq!(fake.applied[0].peers.len(), 2, "node {i}: both peers");
            }
        })
        .await;
}

/// A broken deployed component must never run the native protocol instead.
#[tokio::test]
async fn a_faulting_guest_stops_without_native_effects() {
    for backend in [
        guest_backend(1),
        NetstackBackend::Guest {
            component: b"invalid component".to_vec(),
            step_fuel: reachability::NETSTACK_STEP_FUEL,
        },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let signer = PrivateKey::from_seed(1);
        let policy = PortPolicy::production();
        let config = ReachabilityConfig {
            chain_id: CHAIN.into(),
            signer: signer.clone(),
            wireguard_key_file: dir.path().join("wg.key"),
            wireguard_port: 51820,
            wireguard_advertised: Some(endpoint(&policy, 10, 51820, Transport::Udp)),
            control_endpoint: endpoint(&policy, 10, 443, Transport::Tcp),
            coordinators: vec![],
            port_policy: policy,
            persist_file: None,
            gossip_ingress: None,
            backend,
        };
        let (commands, receiver) = mpsc::channel(8);
        let (events, _received) = mpsc::channel(64);
        commands
            .send(ReachabilityCommand::Retarget(MeshEpochEvent {
                epoch: 1,
                members: vec![signer.public_key()],
                standbys: vec![],
                current_view: 10,
            }))
            .await
            .unwrap();
        drop(commands);
        let effect = SharedFake::default();
        let mut transitions = Vec::new();
        let outcome = reachability::run_observed(
            config,
            effect.clone(),
            StaticResolver::default(),
            receiver,
            events,
            |status| transitions.push(status),
        )
        .await;
        assert_eq!(
            transitions.first(),
            Some(&reachability::BackendStatus::Starting)
        );
        assert!(matches!(
            transitions.last(),
            Some(reachability::BackendStatus::Failed(_))
        ));
        assert_eq!(transitions.last().unwrap().code_hash(), None);
        assert!(
            matches!(outcome, Err(reachability::ReachabilityError::Backend(_))),
            "{outcome:?}"
        );
        assert_eq!(
            effect.0.lock().unwrap().create_calls,
            0,
            "no native retarget replay"
        );
    }
}

/// Ask the node's plane to swap its backend and await the plane's answer.
async fn swap_backend(node: &TestNode, backend: NetstackBackend) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    node.cmd
        .send(ReachabilityCommand::SwapBackend {
            backend,
            reply: SwapReply(tx),
        })
        .await
        .unwrap();
    rx.await.expect("plane alive")
}

/// Ask the owner plane to restore the current snapshot into a candidate and
/// discard that candidate without changing the live execution.
async fn preflight_backend(
    commands: &mpsc::Sender<ReachabilityCommand>,
    backend: NetstackBackend,
) -> Result<(), String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    commands
        .send(ReachabilityCommand::PreflightBackend {
            backend,
            reply: reachability::PreflightReply(tx),
        })
        .await
        .unwrap();
    rx.await.expect("plane alive")
}

/// A backend swap mid-epoch — native to guest with the retarget just
/// stepped, guest back to native once applied — continues the epoch on the
/// swapped machine: one interface, one apply, and the next cutover runs on
/// it like any other.
#[tokio::test]
async fn a_backend_swap_mid_epoch_keeps_the_tunnels() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            // queued right behind the retarget: the epoch is assembling.
            swap_backend(&nodes[0], guest_backend(reachability::NETSTACK_STEP_FUEL))
                .await
                .expect("the guest continues the epoch");
            let versions = await_applied(&mut collected, &[0, 1, 2], 1).await;
            assert_eq!(versions[&0], versions[&1]);
            assert_eq!(versions[&0], versions[&2]);
            for (i, node) in nodes.iter().enumerate() {
                let fake = node.effect.0.lock().unwrap();
                assert_eq!(fake.create_calls, 1, "node {i}: one interface");
                assert_eq!(fake.applied.len(), 1, "node {i}: one apply");
            }

            // and back, on the settled epoch: nothing moves on the interface.
            swap_backend(&nodes[0], NetstackBackend::Native)
                .await
                .expect("the native machine continues the epoch");
            for _ in 0..3 {
                nodes[0].cmd.send(ReachabilityCommand::Nudge).await.unwrap();
            }

            // the next cutover runs on the swapped machine like any other.
            retarget_all(&nodes, &[0, 1, 2], &[], 2, 20).await;
            await_applied(&mut collected, &[0, 1, 2], 2).await;
            let fake = nodes[0].effect.0.lock().unwrap();
            assert_eq!(fake.create_calls, 1, "the interface never flapped");
            assert_eq!(
                fake.applied.len(),
                2,
                "one apply per epoch, none for the swaps"
            );
        })
        .await;
}

/// A backend that cannot restore the snapshot is refused, and the current
/// machine continues untouched.
#[tokio::test]
async fn a_refused_swap_leaves_the_current_machine_in_place() {
    let local = LocalSet::new();
    let dir = tempfile::tempdir().unwrap();
    local
        .run_until(async {
            let (nodes, mut collected) = spawn_mesh(&local, dir.path(), &[1, 2, 3], vec![]);
            retarget_all(&nodes, &[0, 1, 2], &[], 1, 10).await;
            await_applied(&mut collected, &[0, 1, 2], 1).await;

            let not_a_component = NetstackBackend::Guest {
                component: b"not a component".to_vec(),
                step_fuel: reachability::NETSTACK_STEP_FUEL,
            };
            let refusal = swap_backend(&nodes[0], not_a_component)
                .await
                .expect_err("bytes that are not a component cannot take the plane");
            assert!(refusal.contains("component"), "{refusal}");

            // the plane is unchanged: the next epoch converges on the same
            // machine and the same interface.
            retarget_all(&nodes, &[0, 1, 2], &[], 2, 20).await;
            await_applied(&mut collected, &[0, 1, 2], 2).await;
            let fake = nodes[0].effect.0.lock().unwrap();
            assert_eq!(fake.create_calls, 1);
            assert_eq!(fake.applied.len(), 2);
        })
        .await;
}

#[tokio::test]
async fn a_successful_preflight_restores_current_state_without_swapping() {
    use reachability::BackendStatus;
    let dir = tempfile::tempdir().unwrap();
    let policy = PortPolicy::production();
    let config = ReachabilityConfig {
        chain_id: CHAIN.into(),
        signer: PrivateKey::from_seed(1),
        wireguard_key_file: dir.path().join("wg.key"),
        wireguard_port: 51820,
        wireguard_advertised: Some(endpoint(&policy, 10, 51820, Transport::Udp)),
        control_endpoint: endpoint(&policy, 10, 443, Transport::Tcp),
        coordinators: vec![],
        port_policy: policy,
        persist_file: None,
        gossip_ingress: None,
        backend: observable_guest("first"),
    };
    let (commands, receiver) = mpsc::channel(8);
    let (events, mut received) = mpsc::channel(8);
    let (status, mut statuses) = mpsc::unbounded_channel();
    let local = LocalSet::new();
    local
        .run_until(async {
            let running = tokio::task::spawn_local(reachability::run_observed(
                config,
                SharedFake::default(),
                StaticResolver::default(),
                receiver,
                events,
                move |value| {
                    status.send(value).unwrap();
                },
            ));
            assert_eq!(statuses.recv().await.unwrap(), BackendStatus::Starting);
            assert!(statuses.recv().await.unwrap().code_hash().is_some());

            commands.send(ReachabilityCommand::Nudge).await.unwrap();
            assert!(matches!(
                received.recv().await.unwrap(),
                ReachabilityEvent::PersistFailed { ref reason } if reason == "first"
            ));
            preflight_backend(&commands, observable_guest("second"))
                .await
                .expect("candidate restores the current snapshot");
            assert!(
                statuses.try_recv().is_err(),
                "preflight never publishes status"
            );
            commands.send(ReachabilityCommand::Nudge).await.unwrap();
            assert!(matches!(
                received.recv().await.unwrap(),
                ReachabilityEvent::PersistFailed { ref reason } if reason == "first"
            ));
            commands.send(ReachabilityCommand::Shutdown).await.unwrap();
            running.await.unwrap().unwrap();
            assert_eq!(statuses.recv().await.unwrap(), BackendStatus::Stopped);
        })
        .await;
}

#[tokio::test]
async fn an_incompatible_preflight_refuses_without_status_or_machine_mutation() {
    use reachability::BackendStatus;
    let dir = tempfile::tempdir().unwrap();
    let policy = PortPolicy::production();
    let config = ReachabilityConfig {
        chain_id: CHAIN.into(),
        signer: PrivateKey::from_seed(1),
        wireguard_key_file: dir.path().join("wg.key"),
        wireguard_port: 51820,
        wireguard_advertised: Some(endpoint(&policy, 10, 51820, Transport::Udp)),
        control_endpoint: endpoint(&policy, 10, 443, Transport::Tcp),
        coordinators: vec![],
        port_policy: policy,
        persist_file: None,
        gossip_ingress: None,
        backend: observable_guest("first"),
    };
    let (commands, receiver) = mpsc::channel(8);
    let (events, mut received) = mpsc::channel(8);
    let (status, mut statuses) = mpsc::unbounded_channel();
    let local = LocalSet::new();
    local
        .run_until(async {
            let running = tokio::task::spawn_local(reachability::run_observed(
                config,
                SharedFake::default(),
                StaticResolver::default(),
                receiver,
                events,
                move |value| {
                    status.send(value).unwrap();
                },
            ));
            assert_eq!(statuses.recv().await.unwrap(), BackendStatus::Starting);
            assert!(statuses.recv().await.unwrap().code_hash().is_some());
            commands.send(ReachabilityCommand::Nudge).await.unwrap();
            assert!(matches!(
                received.recv().await.unwrap(),
                ReachabilityEvent::PersistFailed { ref reason } if reason == "first"
            ));

            let refusal = preflight_backend(
                &commands,
                NetstackBackend::Guest {
                    component: vec![],
                    step_fuel: 1,
                },
            )
            .await
            .expect_err("an incompatible candidate must be refused");
            assert!(refusal.contains("component"), "{refusal}");
            assert!(
                statuses.try_recv().is_err(),
                "refusal never publishes status"
            );
            commands.send(ReachabilityCommand::Nudge).await.unwrap();
            assert!(matches!(
                received.recv().await.unwrap(),
                ReachabilityEvent::PersistFailed { ref reason } if reason == "first"
            ));
            commands.send(ReachabilityCommand::Shutdown).await.unwrap();
            running.await.unwrap().unwrap();
            assert_eq!(statuses.recv().await.unwrap(), BackendStatus::Stopped);
        })
        .await;
}

/// A component fixture with a distinguishable protocol effect. Its snapshot
/// is opaque to the executor; both revisions intentionally accept that state.
fn observable_guest(marker: &str) -> NetstackBackend {
    let step = netstack_machine::wire::encode_step(&Ok(vec![netstack_machine::Effect::Observe(
        ReachabilityEvent::PersistFailed {
            reason: marker.into(),
        },
    )]));
    let data: String = step.iter().map(|byte| format!("\\{byte:02x}")).collect();
    let length = format!("\\{:02x}", step.len());
    let component = wat::parse_str(format!(r#"(component
      (core module $m
        (memory (export "memory") 1)
        (global $heap (mut i32) (i32.const 4096))
        (func (export "realloc") (param i32 i32 i32 i32) (result i32)
          (local $p i32)
          global.get $heap local.set $p
          global.get $heap local.get 3 i32.add global.set $heap
          local.get $p)
        (data (i32.const 64) "\00\00\00\00\80\00\00\00{length}\00\00\00")
        (data (i32.const 128) "{data}")
        (func (export "configure") (param i32 i32) (result i32) i32.const 0)
        (func (export "restore") (param i32 i32 i32 i32) (result i32) i32.const 0)
        (func (export "step") (param i32 i32 i64) (result i32) i32.const 64)
        (func (export "snapshot") (result i32) i32.const 64))
      (core instance $i (instantiate $m))
      (func (export "configure") (param "config" (list u8)) (result (result (error string)))
        (canon lift (core func $i "configure") (memory $i "memory") (realloc (func $i "realloc"))))
      (func (export "restore") (param "config" (list u8)) (param "snapshot" (list u8)) (result (result (error string)))
        (canon lift (core func $i "restore") (memory $i "memory") (realloc (func $i "realloc"))))
      (func (export "step") (param "event" (list u8)) (param "now-ms" u64) (result (result (list u8) (error string)))
        (canon lift (core func $i "step") (memory $i "memory") (realloc (func $i "realloc"))))
      (func (export "snapshot") (result (result (list u8) (error string)))
        (canon lift (core func $i "snapshot") (memory $i "memory") (realloc (func $i "realloc")))))"#,
        length = length,
    )).unwrap();
    NetstackBackend::Guest {
        component,
        step_fuel: reachability::NETSTACK_STEP_FUEL,
    }
}

#[tokio::test]
async fn replacing_wasm_changes_executed_effects_and_reports_actual_code() {
    use reachability::BackendStatus;
    let dir = tempfile::tempdir().unwrap();
    let policy = PortPolicy::production();
    let config = ReachabilityConfig {
        chain_id: CHAIN.into(),
        signer: PrivateKey::from_seed(1),
        wireguard_key_file: dir.path().join("wg.key"),
        wireguard_port: 51820,
        wireguard_advertised: Some(endpoint(&policy, 10, 51820, Transport::Udp)),
        control_endpoint: endpoint(&policy, 10, 443, Transport::Tcp),
        coordinators: vec![],
        port_policy: policy,
        persist_file: None,
        gossip_ingress: None,
        backend: observable_guest("first"),
    };
    let (commands, receiver) = mpsc::channel(8);
    let (events, mut received) = mpsc::channel(8);
    let (status, mut statuses) = mpsc::unbounded_channel();
    let local = LocalSet::new();
    local.run_until(async {
        let running = tokio::task::spawn_local(reachability::run_observed(
            config, SharedFake::default(), StaticResolver::default(), receiver, events,
            move |value| { status.send(value).unwrap(); },
        ));
        assert_eq!(statuses.recv().await.unwrap(), BackendStatus::Starting);
        let first = statuses.recv().await.unwrap();
        assert!(first.code_hash().is_some());
        commands.send(ReachabilityCommand::Nudge).await.unwrap();
        assert!(matches!(received.recv().await.unwrap(), ReachabilityEvent::PersistFailed { reason } if reason == "first"));

        let (reply, answer) = tokio::sync::oneshot::channel();
        commands.send(ReachabilityCommand::SwapBackend { backend: observable_guest("second"), reply: SwapReply(reply) }).await.unwrap();
        answer.await.unwrap().unwrap();
        let second = statuses.recv().await.unwrap();
        assert_ne!(first.code_hash(), second.code_hash());
        commands.send(ReachabilityCommand::Nudge).await.unwrap();
        assert!(matches!(received.recv().await.unwrap(), ReachabilityEvent::PersistFailed { reason } if reason == "second"));

        let (reply, answer) = tokio::sync::oneshot::channel();
        commands.send(ReachabilityCommand::SwapBackend { backend: NetstackBackend::Guest { component: vec![], step_fuel: 1 }, reply: SwapReply(reply) }).await.unwrap();
        assert!(answer.await.unwrap().is_err());
        assert!(statuses.try_recv().is_err(), "a refused candidate never becomes running");
        commands.send(ReachabilityCommand::Nudge).await.unwrap();
        assert!(matches!(received.recv().await.unwrap(), ReachabilityEvent::PersistFailed { reason } if reason == "second"));
        commands.send(ReachabilityCommand::Shutdown).await.unwrap();
        running.await.unwrap().unwrap();
        assert_eq!(statuses.recv().await.unwrap(), BackendStatus::Stopped);
    }).await;
}
