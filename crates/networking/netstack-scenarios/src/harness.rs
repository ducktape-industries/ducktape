//! The lifecycle harness: N machines on a scripted network, a modeled
//! resolver, a simulated clock, and a trace recorder — no runtime, no
//! sockets, no sleeps. Every wait is an item on one discrete-event queue,
//! so a run is a pure function of the script, and the trace it records is
//! the canonical event→effect log the fixtures freeze.
//!
//! What the harness performs, mirroring the host executor exactly: a
//! `MeshSend` becomes the target's `Deliver` after the link's delay (or a
//! recorded drop); a `WgApply` round-trips synchronously; a resolver start
//! is answered from the scenario's model after its latency; `Persist`
//! bytes are kept per node and offered back on the first retarget after a
//! restart, the way the executor offers the state file.
//!
//! The machine behind each node is whatever the scenario's [`Backend`]
//! builds — the trace never says which, which is the point.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use nat_traversal::NodeKey;
use netstack_machine::msg::ReachabilityMsg;
use netstack_machine::{
    CmdToken, Effect, Event, MachineConfig, MeshEpochEvent, NetstackMachine, ReachabilityEvent,
    ReqId, Resolution, binding,
};
use sha2::{Digest, Sha256};
use wireguard::effect::PeerTunnelConfig;
use wireguard::{Endpoint, MeshVersion, PortPolicy, Transport, ValidatorIdentity, X25519PublicKey};

use crate::Backend;

/// One hop on the scripted network.
pub const LINK_DELAY_MS: u64 = 10;
/// A modeled resolver answer, in simulated time.
pub const RESOLVE_LATENCY_MS: u64 = 50;
/// The node's nudge cadence, mirrored by [`Net::nudges`].
pub const NUDGE_PERIOD_MS: u64 = 2_000;
/// The clock every scenario starts at (a small, readable stamp; the
/// machine only ever needs it monotonic).
pub const T0_MS: u64 = 1_000;

const WIREGUARD_PORT: u16 = 51_820;
const CONTROL_PORT: u16 = 443;

/// A reachability message's kind, for loss rules and trace lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgKind {
    Record,
    Advert,
    Request,
    Response,
    Ack,
}

/// A loss rule on one directed link, consumed as it fires.
#[derive(Clone, Debug)]
pub enum Loss {
    /// Drop the next `n` messages of any kind.
    Next(u32),
    /// Drop the next `n` messages of this kind.
    Kind(MsgKind, u32),
}

/// One directed link's behavior.
#[derive(Clone, Debug)]
pub struct Link {
    pub delay_ms: u64,
    pub up: bool,
    pub duplicate: bool,
    pub drops: Vec<Loss>,
}

impl Link {
    pub fn direct() -> Self {
        Self {
            delay_ms: LINK_DELAY_MS,
            up: true,
            duplicate: false,
            drops: Vec::new(),
        }
    }

    pub fn dropping(drops: Vec<Loss>) -> Self {
        Self {
            drops,
            ..Self::direct()
        }
    }

    pub fn duplicating() -> Self {
        Self {
            duplicate: true,
            ..Self::direct()
        }
    }
}

/// How one node joins the network: its key seed and address octet, and
/// whether it advertises a dialable WireGuard endpoint at all.
#[derive(Clone, Copy, Debug)]
pub struct NodeSpec {
    pub octet: u8,
    pub advertised: bool,
}

impl NodeSpec {
    pub fn public(octet: u8) -> Self {
        Self {
            octet,
            advertised: true,
        }
    }

    pub fn endpoint_less(octet: u8) -> Self {
        Self {
            octet,
            advertised: false,
        }
    }
}

/// A host command's answer, as the machine replied it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Install(Result<(), String>),
    Intro(Result<Vec<u8>, String>),
}

/// A modeled resolver outcome and how long the resolver takes to say it.
#[derive(Clone, Debug)]
pub struct Answer<T> {
    pub outcome: Result<T, String>,
    pub latency_ms: u64,
}

impl<T> Answer<T> {
    pub fn ok(value: T) -> Self {
        Self {
            outcome: Ok(value),
            latency_ms: RESOLVE_LATENCY_MS,
        }
    }

    pub fn err(reason: &str) -> Self {
        Self {
            outcome: Err(reason.into()),
            latency_ms: RESOLVE_LATENCY_MS,
        }
    }
}

struct Node {
    name: String,
    signer: PrivateKey,
    key: PublicKey,
    identity: ValidatorIdentity,
    node_key: NodeKey,
    wg: X25519PublicKey,
    advertised: Option<Endpoint>,
    control: Endpoint,
    /// `None` while the node is down.
    machine: Option<Box<dyn NetstackMachine>>,
    /// The state file this life will offer on its first retarget.
    restore: Option<Vec<u8>>,
    /// The last persisted snapshot (what a restart reads back).
    persisted: Option<Vec<u8>>,
    observed: Vec<ReachabilityEvent>,
    /// The peer set of the node's last accepted interface push — the live
    /// interface as the host would hold it.
    interface: Vec<PeerTunnelConfig>,
}

enum Pending {
    Deliver {
        to: usize,
        from: PublicKey,
        bytes: Vec<u8>,
    },
    Resolved {
        node: usize,
        req: ReqId,
        outcome: Result<Resolution, String>,
    },
    Rendezvous {
        node: usize,
        req: ReqId,
        outcome: Result<SocketAddr, String>,
    },
    Datagram {
        node: usize,
        req: ReqId,
        outcome: Result<Vec<u8>, String>,
    },
    Nudge {
        node: usize,
    },
}

impl Pending {
    fn node(&self) -> usize {
        match self {
            Pending::Deliver { to, .. } => *to,
            Pending::Resolved { node, .. }
            | Pending::Rendezvous { node, .. }
            | Pending::Datagram { node, .. }
            | Pending::Nudge { node } => *node,
        }
    }
}

struct Scheduled {
    due: u64,
    seq: u64,
    item: Pending,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.seq == other.seq
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scheduled {
    // a min-heap on (due, seq): earliest first, FIFO among equals.
    fn cmp(&self, other: &Self) -> Ordering {
        (other.due, other.seq).cmp(&(self.due, self.seq))
    }
}

/// The whole simulated world.
pub struct Net {
    scenario: String,
    backend: Backend,
    chain_id: String,
    coordinators: Vec<SocketAddr>,
    persist: bool,
    connected_by_default: bool,
    nodes: Vec<Node>,
    links: BTreeMap<(usize, usize), Link>,
    /// `(resolver node, peer)` → the modeled answer; `None` = never answers.
    resolves: BTreeMap<(usize, usize), Option<Answer<Resolution>>>,
    rendezvous: BTreeMap<(usize, usize), Option<Answer<SocketAddr>>>,
    /// What a node's awaited intro datagram gets back.
    datagram_replies: BTreeMap<usize, Answer<Vec<u8>>>,
    /// Every host command's reply, by the token the scenario minted for it.
    replies: BTreeMap<CmdToken, Reply>,
    queue: BinaryHeap<Scheduled>,
    now_ms: u64,
    seq: u64,
    next_token: u64,
    trace: String,
}

impl Net {
    /// `scenario` names the fixture (`fixtures/<scenario>.trace`); `backend`
    /// builds every node's machine.
    pub fn new(scenario: &str, chain_id: &str, specs: &[NodeSpec], backend: Backend) -> Self {
        let policy = PortPolicy::production();
        let nodes = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let signer = PrivateKey::from_seed(u64::from(spec.octet));
                let key = signer.public_key();
                let identity = binding::identity_of(&key);
                let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, spec.octet));
                let endpoint =
                    |port, transport| Endpoint::new(ip, port, transport, &policy).unwrap();
                Node {
                    name: format!("n{}", index + 1),
                    key,
                    identity,
                    node_key: binding::node_key(identity),
                    signer,
                    wg: X25519PublicKey([spec.octet; 32]),
                    advertised: spec
                        .advertised
                        .then(|| endpoint(WIREGUARD_PORT, Transport::Udp)),
                    control: endpoint(CONTROL_PORT, Transport::Tcp),
                    machine: None,
                    restore: None,
                    persisted: None,
                    observed: Vec::new(),
                    interface: Vec::new(),
                }
            })
            .collect();
        let mut net = Self {
            scenario: scenario.into(),
            backend,
            chain_id: chain_id.into(),
            coordinators: Vec::new(),
            persist: false,
            connected_by_default: true,
            nodes,
            links: BTreeMap::new(),
            resolves: BTreeMap::new(),
            rendezvous: BTreeMap::new(),
            datagram_replies: BTreeMap::new(),
            replies: BTreeMap::new(),
            queue: BinaryHeap::new(),
            now_ms: T0_MS,
            seq: 0,
            next_token: 0,
            trace: String::new(),
        };
        let _ = writeln!(net.trace, "chain {chain_id}");
        for (index, spec) in specs.iter().enumerate() {
            let node = &net.nodes[index];
            let advertised = match &node.advertised {
                Some(endpoint) => endpoint.socket_addr().to_string(),
                None => "endpoint-less".into(),
            };
            let _ = writeln!(
                net.trace,
                "{} = seed {} id {} wg {}",
                node.name,
                spec.octet,
                short(node.identity),
                advertised
            );
        }
        net
    }

    /// Rendezvous coordinators exist (the machine then rendezvouses
    /// endpoint-less peers by identity through the modeled resolver).
    pub fn with_coordinators(mut self) -> Self {
        self.coordinators = vec![SocketAddr::from(([8, 8, 8, 1], 3478))];
        self
    }

    /// Nodes persist their applied mesh and restore it after a restart.
    pub fn with_persistence(mut self) -> Self {
        self.persist = true;
        self
    }

    /// No link exists unless [`Net::connect`] / [`Net::link`] creates it.
    pub fn isolated(mut self) -> Self {
        self.connected_by_default = false;
        self
    }

    pub fn link(&mut self, from: usize, to: usize, link: Link) {
        self.links.insert((from, to), link);
    }

    pub fn connect(&mut self, a: usize, b: usize) {
        self.link(a, b, Link::direct());
        self.link(b, a, Link::direct());
    }

    /// Cut both directions between `a` and `b`. Messages already past the
    /// link still land; everything sent from now on is dropped.
    pub fn partition(&mut self, a: usize, b: usize) {
        self.set_link_up(a, b, false);
        self.set_link_up(b, a, false);
    }

    /// Restore both directions between `a` and `b`.
    pub fn reconnect(&mut self, a: usize, b: usize) {
        self.set_link_up(a, b, true);
        self.set_link_up(b, a, true);
    }

    fn set_link_up(&mut self, from: usize, to: usize, up: bool) {
        self.links.entry((from, to)).or_insert_with(Link::direct).up = up;
    }

    pub fn resolve_answer(&mut self, node: usize, peer: usize, answer: Option<Answer<Resolution>>) {
        self.resolves.insert((node, peer), answer);
    }

    pub fn rendezvous_answer(
        &mut self,
        node: usize,
        peer: usize,
        answer: Option<Answer<SocketAddr>>,
    ) {
        self.rendezvous.insert((node, peer), answer);
    }

    pub fn datagram_reply(&mut self, node: usize, answer: Answer<Vec<u8>>) {
        self.datagram_replies.insert(node, answer);
    }

    /// Re-home a node's advertised WireGuard endpoint (takes effect on its
    /// next life).
    pub fn set_advertised(&mut self, node: usize, octet: u8) {
        let policy = PortPolicy::production();
        let ip = IpAddr::V4(Ipv4Addr::new(8, 8, 8, octet));
        self.nodes[node].advertised =
            Some(Endpoint::new(ip, WIREGUARD_PORT, Transport::Udp, &policy).unwrap());
    }

    pub fn key(&self, node: usize) -> PublicKey {
        self.nodes[node].key.clone()
    }

    /// Does `node`'s live interface carry a tunnel to `peer`? The peer set
    /// of the last accepted push — what the host's WireGuard interface
    /// holds, and so what an inbound handshake initiation from `peer`
    /// would be answered against.
    pub fn tunneled(&self, node: usize, peer: usize) -> bool {
        let key = self.nodes[peer].wg;
        self.nodes[node]
            .interface
            .iter()
            .any(|entry| entry.wireguard_public_key == key)
    }

    pub fn saw(&self, node: usize, pred: impl Fn(&ReachabilityEvent) -> bool) -> bool {
        self.nodes[node].observed.iter().any(pred)
    }

    /// How many of the node's observations satisfy `pred`.
    pub fn count(&self, node: usize, pred: impl Fn(&ReachabilityEvent) -> bool) -> usize {
        self.nodes[node].observed.iter().filter(|e| pred(e)).count()
    }

    /// The machine's reply to the command `token` names, once it has one.
    pub fn reply(&self, token: CmdToken) -> Option<&Reply> {
        self.replies.get(&token)
    }

    fn config(&self, node: usize) -> MachineConfig {
        let node = &self.nodes[node];
        MachineConfig {
            chain_id: self.chain_id.clone(),
            wireguard_public: node.wg,
            wireguard_advertised: node.advertised,
            control_endpoint: node.control,
            coordinators: self.coordinators.clone(),
            port_policy: PortPolicy::production(),
            persist: self.persist,
            gossip_ingress: None,
        }
    }

    /// Bring a node up (first boot, or a restart that reads back what it
    /// last persisted). Anything in flight toward the node is gone with
    /// its old life — the transport links died with the process.
    pub fn start(&mut self, node: usize) {
        self.purge(node);
        let machine =
            (self.backend.build)(Box::new(self.nodes[node].signer.clone()), self.config(node));
        let entry = &mut self.nodes[node];
        entry.machine = Some(machine);
        entry.restore = entry.persisted.clone();
        let _ = writeln!(self.trace, "@{:<6} {} ** start", self.now_ms, entry.name);
    }

    pub fn stop(&mut self, node: usize) {
        self.purge(node);
        self.nodes[node].machine = None;
        let _ = writeln!(
            self.trace,
            "@{:<6} {} ** stop",
            self.now_ms, self.nodes[node].name
        );
    }

    pub fn restart(&mut self, node: usize) {
        self.stop(node);
        self.start(node);
    }

    /// Swap the node's machine for one the backend restores from its
    /// snapshot — the mid-life backend swap. Nothing in flight is touched:
    /// the links, the queue, and the persisted state all outlive the
    /// machine. The trace pins the snapshot's digest, so every backend
    /// must take the SAME bytes from the same state.
    pub fn swap(&mut self, node: usize) {
        let mut machine = self.nodes[node]
            .machine
            .take()
            .expect("a swap needs a running node");
        let snapshot = machine
            .snapshot()
            .expect("a scenario snapshot never faults");
        let _ = writeln!(
            self.trace,
            "@{:<6} {} ** swap snapshot={} ({} bytes)",
            self.now_ms,
            self.nodes[node].name,
            short_hash(Sha256::digest(&snapshot).into()),
            snapshot.len()
        );
        let restored = (self.backend.restore)(
            Box::new(self.nodes[node].signer.clone()),
            self.config(node),
            &snapshot,
        );
        self.nodes[node].machine = Some(restored);
    }

    fn purge(&mut self, node: usize) {
        let kept: Vec<Scheduled> = std::mem::take(&mut self.queue)
            .into_iter()
            .filter(|scheduled| scheduled.item.node() != node)
            .collect();
        self.queue = kept.into_iter().collect();
    }

    fn mesh_event(
        &self,
        epoch: u64,
        members: &[usize],
        standbys: &[usize],
        view: u64,
    ) -> MeshEpochEvent {
        MeshEpochEvent {
            epoch,
            members: members.iter().map(|&m| self.key(m)).collect(),
            standbys: standbys.iter().map(|&s| self.key(s)).collect(),
            current_view: view,
        }
    }

    /// Retarget one node; a node that is not up is started first.
    pub fn retarget(
        &mut self,
        node: usize,
        epoch: u64,
        members: &[usize],
        standbys: &[usize],
        view: u64,
    ) {
        if self.nodes[node].machine.is_none() {
            self.start(node);
        }
        let event = self.mesh_event(epoch, members, standbys, view);
        let persisted = self.nodes[node].restore.take();
        self.drive(node, Event::Retarget { event, persisted });
    }

    /// Retarget every participant (members and standbys) to one epoch.
    pub fn retarget_all(&mut self, epoch: u64, members: &[usize], standbys: &[usize], view: u64) {
        for &node in members.iter().chain(standbys) {
            self.retarget(node, epoch, members, standbys, view);
        }
    }

    /// `rounds` nudge ticks for every live node, [`NUDGE_PERIOD_MS`] apart,
    /// starting one period from now.
    pub fn nudges(&mut self, rounds: u64) {
        for round in 1..=rounds {
            let due = self.now_ms + round * NUDGE_PERIOD_MS;
            for node in 0..self.nodes.len() {
                if self.nodes[node].machine.is_some() {
                    self.schedule(due, Pending::Nudge { node });
                }
            }
        }
    }

    /// This node's own NAT rebound mid-epoch: the host observed a new
    /// reflexive for itself. Scripts a LIVE rebind — no restart, the epoch
    /// and every peer's session stay exactly where they are.
    pub fn reflexive_changed(&mut self, node: usize, endpoint: SocketAddr) {
        self.drive(node, Event::ReflexiveChanged { endpoint });
    }

    pub fn view_tick_all(&mut self, view: u64) {
        for node in 0..self.nodes.len() {
            if self.nodes[node].machine.is_some() {
                self.drive(node, Event::ViewTick(view));
            }
        }
    }

    pub fn install_invite_peer(
        &mut self,
        node: usize,
        peer: usize,
        endpoint: SocketAddr,
    ) -> CmdToken {
        let token = self.mint_token();
        let event = Event::InstallInvitePeer {
            token,
            peer: self.key(peer),
            wireguard_public_key: self.nodes[peer].wg,
            endpoint,
        };
        self.drive(node, event);
        token
    }

    pub fn bootstrap_coordinated_invite(
        &mut self,
        node: usize,
        inviter: usize,
        intro: Vec<u8>,
    ) -> CmdToken {
        let token = self.mint_token();
        let event = Event::BootstrapCoordinatedInvitePeer {
            token,
            peer: self.key(inviter),
            wireguard_public_key: self.nodes[inviter].wg,
            intro,
        };
        self.drive(node, event);
        token
    }

    pub fn shutdown(&mut self, node: usize) {
        self.drive(node, Event::Shutdown);
        self.nodes[node].machine = None;
    }

    fn mint_token(&mut self) -> CmdToken {
        self.next_token += 1;
        CmdToken(self.next_token)
    }

    fn schedule(&mut self, due: u64, item: Pending) {
        self.seq += 1;
        self.queue.push(Scheduled {
            due,
            seq: self.seq,
            item,
        });
    }

    /// Run the queue dry.
    pub fn run(&mut self) {
        self.run_until(u64::MAX);
    }

    /// Run every item due up to and including `until_ms`, then set the
    /// clock there (so a scenario can script "and then, at T, …").
    pub fn run_until(&mut self, until_ms: u64) {
        while let Some(next) = self.queue.peek() {
            if next.due > until_ms {
                break;
            }
            let Scheduled { due, item, .. } = self.queue.pop().expect("peeked");
            self.now_ms = self.now_ms.max(due);
            self.perform(item);
        }
        if until_ms != u64::MAX {
            self.now_ms = self.now_ms.max(until_ms);
        }
    }

    fn perform(&mut self, item: Pending) {
        match item {
            Pending::Deliver { to, from, bytes } => self.drive(to, Event::Deliver { from, bytes }),
            Pending::Resolved { node, req, outcome } => {
                self.drive(node, Event::Resolved { req, outcome })
            }
            Pending::Rendezvous { node, req, outcome } => {
                self.drive(node, Event::RendezvousResolved { req, outcome })
            }
            Pending::Datagram { node, req, outcome } => {
                self.drive(node, Event::DatagramReplied { req, outcome })
            }
            Pending::Nudge { node } => self.drive(node, Event::Nudge),
        }
    }

    /// Step one machine and perform its effects in order, the way the host
    /// executor does. A node that is down drops the event (its process is
    /// not there to receive it).
    fn drive(&mut self, node: usize, event: Event) {
        let Some(mut machine) = self.nodes[node].machine.take() else {
            let _ = writeln!(
                self.trace,
                "@{:<6} {} <- {} (dropped: down)",
                self.now_ms,
                self.nodes[node].name,
                self.render_event(&event)
            );
            return;
        };
        let mut stack = vec![self.step(&mut machine, node, event).into_iter()];
        while let Some(top) = stack.last_mut() {
            let Some(effect) = top.next() else {
                stack.pop();
                continue;
            };
            let line = self.render_effect(&effect);
            let _ = writeln!(self.trace, "        {} -> {line}", self.nodes[node].name);
            match effect {
                Effect::MeshSend { to, bytes } => self.route(node, to, bytes),
                Effect::Observe(observed) => self.nodes[node].observed.push(observed),
                Effect::WgApply { req, peers, .. } => {
                    self.nodes[node].interface = peers;
                    let more = self.step(
                        &mut machine,
                        node,
                        Event::WgApplied {
                            req,
                            outcome: Ok(()),
                        },
                    );
                    stack.push(more.into_iter());
                }
                Effect::WgRemove => self.nodes[node].interface.clear(),
                Effect::ResolveStart { req, peer, .. } => {
                    let target = self.node_by_key(peer);
                    let answer = target
                        .and_then(|t| self.resolves.get(&(node, t)).cloned())
                        .unwrap_or(Some(Answer::ok(Resolution::Advertised)));
                    if let Some(answer) = answer {
                        let due = self.now_ms + answer.latency_ms;
                        self.schedule(
                            due,
                            Pending::Resolved {
                                node,
                                req,
                                outcome: answer.outcome,
                            },
                        );
                    }
                }
                Effect::RendezvousStart { req, peer } => {
                    let target = self.node_by_key(peer);
                    let answer = target
                        .and_then(|t| self.rendezvous.get(&(node, t)).cloned())
                        .unwrap_or(Some(Answer::err("no coordinator answered")));
                    if let Some(answer) = answer {
                        let due = self.now_ms + answer.latency_ms;
                        self.schedule(
                            due,
                            Pending::Rendezvous {
                                node,
                                req,
                                outcome: answer.outcome,
                            },
                        );
                    }
                }
                Effect::UdpSend { .. } => {}
                Effect::UdpSendAwait {
                    req, timeout_ms, ..
                } => {
                    let answer = self.datagram_replies.get(&node).cloned().unwrap_or(Answer {
                        outcome: Err("intro ack timed out".into()),
                        latency_ms: timeout_ms,
                    });
                    let due = self.now_ms + answer.latency_ms;
                    self.schedule(
                        due,
                        Pending::Datagram {
                            node,
                            req,
                            outcome: answer.outcome,
                        },
                    );
                }
                Effect::ReplyInstall { token, outcome } => {
                    self.replies.insert(token, Reply::Install(outcome));
                }
                Effect::ReplyIntro { token, outcome } => {
                    self.replies.insert(token, Reply::Intro(outcome));
                }
                Effect::Persist { bytes } => self.nodes[node].persisted = Some(bytes),
            }
        }
        self.nodes[node].machine = Some(machine);
    }

    fn step(
        &mut self,
        machine: &mut dyn NetstackMachine,
        node: usize,
        event: Event,
    ) -> Vec<Effect> {
        let _ = writeln!(
            self.trace,
            "@{:<6} {} <- {}",
            self.now_ms,
            self.nodes[node].name,
            self.render_event(&event)
        );
        machine
            .step(event, self.now_ms)
            .expect("a scenario step never breaches a protocol invariant or faults")
    }

    /// Route one mesh send through the scripted link, recording a drop
    /// where the link refuses it.
    fn route(&mut self, from: usize, to: PublicKey, bytes: Vec<u8>) {
        let Some(target) = self.nodes.iter().position(|n| n.key == to) else {
            let _ = writeln!(self.trace, "           -- dropped: unknown peer");
            return;
        };
        if self.nodes[target].machine.is_none() {
            let _ = writeln!(
                self.trace,
                "           -- dropped: {} is down",
                self.nodes[target].name
            );
            return;
        }
        let known = self.links.contains_key(&(from, target));
        if !known && !self.connected_by_default {
            let _ = writeln!(
                self.trace,
                "           -- dropped: no link to {}",
                self.nodes[target].name
            );
            return;
        }
        let kind = Self::msg_kind(&bytes);
        let link = self
            .links
            .entry((from, target))
            .or_insert_with(Link::direct);
        let verdict = match link.up {
            false => Some("partitioned".to_string()),
            true => consume_drop(&mut link.drops, kind),
        };
        let (delay_ms, duplicate) = (link.delay_ms, link.duplicate);
        if let Some(rule) = verdict {
            let _ = writeln!(
                self.trace,
                "           -- dropped to {}: {rule}",
                self.nodes[target].name
            );
            return;
        }
        let due = self.now_ms + delay_ms;
        let from_key = self.nodes[from].key.clone();
        if duplicate {
            let _ = writeln!(
                self.trace,
                "           -- duplicated to {}",
                self.nodes[target].name
            );
            self.schedule(
                due,
                Pending::Deliver {
                    to: target,
                    from: from_key.clone(),
                    bytes: bytes.clone(),
                },
            );
        }
        self.schedule(
            due,
            Pending::Deliver {
                to: target,
                from: from_key,
                bytes,
            },
        );
    }

    fn node_by_key(&self, key: NodeKey) -> Option<usize> {
        self.nodes.iter().position(|n| n.node_key == key)
    }

    fn name_of_identity(&self, identity: ValidatorIdentity) -> String {
        self.nodes
            .iter()
            .find(|n| n.identity == identity)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| short(identity))
    }

    fn name_of_key(&self, key: &PublicKey) -> String {
        self.name_of_identity(binding::identity_of(key))
    }

    fn name_of_wg(&self, wg: X25519PublicKey) -> String {
        self.nodes
            .iter()
            .find(|n| n.wg == wg)
            .map(|n| n.name.clone())
            .unwrap_or_else(|| format!("wg:{:02x}{:02x}", wg.0[0], wg.0[1]))
    }

    fn name_of_node_key(&self, key: NodeKey) -> String {
        self.node_by_key(key)
            .map(|i| self.nodes[i].name.clone())
            .unwrap_or_else(|| format!("nk:{:02x}{:02x}", key.0[0], key.0[1]))
    }

    fn names(&self, keys: &[PublicKey]) -> String {
        keys.iter()
            .map(|k| self.name_of_key(k))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn describe_msg(&self, bytes: &[u8]) -> String {
        match ReachabilityMsg::decode(bytes) {
            Ok(ReachabilityMsg::Record(signed)) => format!(
                "Record({}#{} ep={})",
                self.name_of_identity(signed.record.validator_identity),
                signed.record.nonce,
                signed.record.epoch
            ),
            Ok(ReachabilityMsg::Advert(advert)) => format!(
                "Advert({}#{} v={})",
                self.name_of_identity(advert.record.validator_identity),
                advert.record.nonce,
                short_version(advert.mesh_version)
            ),
            Ok(ReachabilityMsg::Request(request)) => format!(
                "Request({}->{} #{})",
                self.name_of_identity(request.fields.initiator_identity),
                self.name_of_identity(request.fields.responder_identity),
                request.fields.nonce
            ),
            Ok(ReachabilityMsg::Response(response)) => format!(
                "Response({}->{} req={})",
                self.name_of_identity(response.fields.responder_identity),
                self.name_of_identity(response.fields.initiator_identity),
                short_hash(response.fields.request_hash)
            ),
            Ok(ReachabilityMsg::Ack(ack)) => format!(
                "Ack(req={} resp={})",
                short_hash(ack.fields.request_hash),
                short_hash(ack.fields.response_hash)
            ),
            Err(_) => format!("<{} undecodable bytes>", bytes.len()),
        }
    }

    fn msg_kind(bytes: &[u8]) -> Option<MsgKind> {
        match ReachabilityMsg::decode(bytes).ok()? {
            ReachabilityMsg::Record(_) => Some(MsgKind::Record),
            ReachabilityMsg::Advert(_) => Some(MsgKind::Advert),
            ReachabilityMsg::Request(_) => Some(MsgKind::Request),
            ReachabilityMsg::Response(_) => Some(MsgKind::Response),
            ReachabilityMsg::Ack(_) => Some(MsgKind::Ack),
        }
    }

    fn render_event(&self, event: &Event) -> String {
        match event {
            Event::Retarget { event, persisted } => format!(
                "Retarget(ep={} members=[{}] standbys=[{}] view={} persisted={})",
                event.epoch,
                self.names(&event.members),
                self.names(&event.standbys),
                event.current_view,
                if persisted.is_some() { "yes" } else { "no" }
            ),
            Event::Deliver { from, bytes } => {
                format!(
                    "Deliver(from={} {})",
                    self.name_of_key(from),
                    self.describe_msg(bytes)
                )
            }
            Event::ViewTick(view) => format!("ViewTick({view})"),
            Event::ReflexiveChanged { endpoint } => format!("ReflexiveChanged({endpoint})"),
            Event::Nudge => "Nudge".into(),
            Event::InstallInvitePeer {
                token,
                peer,
                endpoint,
                ..
            } => format!(
                "InstallInvitePeer(token={} peer={} endpoint={endpoint})",
                token.0,
                self.name_of_key(peer)
            ),
            Event::BootstrapCoordinatedInvitePeer {
                token, peer, intro, ..
            } => format!(
                "BootstrapCoordinatedInvitePeer(token={} inviter={} intro={}b)",
                token.0,
                self.name_of_key(peer),
                intro.len()
            ),
            Event::SendResolverDatagram { endpoint, bytes } => {
                format!("SendResolverDatagram(to={endpoint} {}b)", bytes.len())
            }
            Event::Resolved { req, outcome } => format!(
                "Resolved(req={} {})",
                req.0,
                match outcome {
                    Ok(Resolution::Advertised) => "advertised".to_string(),
                    Ok(Resolution::Punched(addr)) => format!("punched {addr}"),
                    Err(reason) => format!("err {reason:?}"),
                }
            ),
            Event::RendezvousResolved { req, outcome } => format!(
                "RendezvousResolved(req={} {})",
                req.0,
                match outcome {
                    Ok(addr) => format!("ok {addr}"),
                    Err(reason) => format!("err {reason:?}"),
                }
            ),
            Event::DatagramReplied { req, outcome } => format!(
                "DatagramReplied(req={} {})",
                req.0,
                match outcome {
                    Ok(bytes) => format!("ok {}b", bytes.len()),
                    Err(reason) => format!("err {reason:?}"),
                }
            ),
            Event::WgApplied { req, outcome } => format!(
                "WgApplied(req={} {})",
                req.0,
                match outcome {
                    Ok(()) => "ok".to_string(),
                    Err(reason) => format!("err {reason:?}"),
                }
            ),
            Event::Shutdown => "Shutdown".into(),
        }
    }

    fn render_peer(&self, peer: &PeerTunnelConfig) -> String {
        format!(
            "{}@{}{}",
            self.name_of_wg(peer.wireguard_public_key),
            peer.endpoint
                .map(|e| e.to_string())
                .unwrap_or_else(|| "-".into()),
            peer.keepalive_seconds
                .map(|k| format!(" ka={k}"))
                .unwrap_or_default()
        )
    }

    fn render_effect(&self, effect: &Effect) -> String {
        match effect {
            Effect::MeshSend { to, bytes } => {
                format!(
                    "MeshSend(to={} {})",
                    self.name_of_key(to),
                    self.describe_msg(bytes)
                )
            }
            Effect::Observe(observed) => format!("Observe({})", self.render_observation(observed)),
            Effect::WgApply {
                req,
                bring_up,
                peers,
            } => format!(
                "WgApply(req={} {} peers=[{}])",
                req.0,
                if *bring_up { "up" } else { "update" },
                peers
                    .iter()
                    .map(|p| self.render_peer(p))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Effect::WgRemove => "WgRemove".into(),
            Effect::ResolveStart {
                req,
                peer,
                advertised,
            } => format!(
                "ResolveStart(req={} peer={} advertised={advertised})",
                req.0,
                self.name_of_node_key(*peer)
            ),
            Effect::RendezvousStart { req, peer } => {
                format!(
                    "RendezvousStart(req={} peer={})",
                    req.0,
                    self.name_of_node_key(*peer)
                )
            }
            Effect::UdpSend { endpoint, bytes } => {
                format!("UdpSend(to={endpoint} {}b)", bytes.len())
            }
            Effect::UdpSendAwait {
                req,
                endpoint,
                bytes,
                timeout_ms,
            } => format!(
                "UdpSendAwait(req={} to={endpoint} {}b timeout={timeout_ms})",
                req.0,
                bytes.len()
            ),
            Effect::ReplyInstall { token, outcome } => format!(
                "ReplyInstall(token={} {})",
                token.0,
                match outcome {
                    Ok(()) => "ok".to_string(),
                    Err(reason) => format!("err {reason:?}"),
                }
            ),
            Effect::ReplyIntro { token, outcome } => format!(
                "ReplyIntro(token={} {})",
                token.0,
                match outcome {
                    Ok(bytes) => format!("ok {}b", bytes.len()),
                    Err(reason) => format!("err {reason:?}"),
                }
            ),
            Effect::Persist { .. } => "Persist".into(),
        }
    }

    fn render_observation(&self, observed: &ReachabilityEvent) -> String {
        match observed {
            ReachabilityEvent::Send { to, bytes } => {
                format!(
                    "Send(to={} {})",
                    self.name_of_key(to),
                    self.describe_msg(bytes)
                )
            }
            ReachabilityEvent::MeshReady { epoch, version } => {
                format!("MeshReady ep={epoch} v={}", short_version(*version))
            }
            ReachabilityEvent::TunnelsApplied { epoch, peers, .. } => {
                format!("TunnelsApplied ep={epoch} peers={peers}")
            }
            ReachabilityEvent::PeerFailed { peer, reason } => {
                format!("PeerFailed {} {reason:?}", self.name_of_key(peer))
            }
            ReachabilityEvent::EpochFailed { epoch, reason } => {
                format!("EpochFailed ep={epoch} {reason:?}")
            }
            ReachabilityEvent::MeshRestored { epoch, peers, .. } => {
                format!("MeshRestored ep={epoch} peers={peers}")
            }
            ReachabilityEvent::RestoreFailed { reason } => format!("RestoreFailed {reason:?}"),
            ReachabilityEvent::PersistFailed { reason } => format!("PersistFailed {reason:?}"),
            ReachabilityEvent::StandbyTunnelsApplied { epoch, peers, .. } => {
                format!("StandbyTunnelsApplied ep={epoch} peers={peers}")
            }
            ReachabilityEvent::MeshAdopted {
                epoch,
                version,
                peers,
            } => {
                format!(
                    "MeshAdopted ep={epoch} v={} peers={peers}",
                    short_version(*version)
                )
            }
            ReachabilityEvent::PeerReadvertised { peer, .. } => {
                format!("PeerReadvertised {}", self.name_of_key(peer))
            }
            ReachabilityEvent::PeerEndpointResolved { peer, endpoint } => {
                format!("PeerEndpointResolved {} {endpoint}", self.name_of_key(peer))
            }
            ReachabilityEvent::InvitePeerInstalled { peer, .. } => {
                format!("InvitePeerInstalled {}", self.name_of_key(peer))
            }
            ReachabilityEvent::ControlEndpointObserved {
                peer,
                control_endpoint,
            } => {
                format!(
                    "ControlEndpointObserved {} {control_endpoint}",
                    self.name_of_identity(*peer)
                )
            }
        }
    }

    /// Compare the recorded trace to the scenario's fixture. A mismatch
    /// names the first divergent line — the fixture diff in the PR is the
    /// review. With `UPDATE_TRACES=1` nothing is compared: the fixture is
    /// rewritten when the net drops (so a scenario whose invariant
    /// assertion fails still leaves its trace behind to read).
    pub fn finish(self) {
        if updating_traces() {
            return;
        }
        let scenario = &self.scenario;
        let path = fixture_path(scenario);
        let pinned = std::fs::read_to_string(&path).unwrap_or_default();
        if pinned == self.trace {
            return;
        }
        let divergence = pinned
            .lines()
            .zip(self.trace.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| pinned.lines().count().min(self.trace.lines().count()));
        let pinned_line = pinned.lines().nth(divergence).unwrap_or("<end of fixture>");
        let actual_line = self
            .trace
            .lines()
            .nth(divergence)
            .unwrap_or("<end of trace>");
        panic!(
            "scenario `{scenario}` diverged from {} at line {}:\n  fixture: {pinned_line}\n  actual:  {actual_line}\n\
             (fixture {} lines, trace {} lines) — a behavior change; regenerate with UPDATE_TRACES=1 \
             and review the fixture diff",
            path.display(),
            divergence + 1,
            pinned.lines().count(),
            self.trace.lines().count()
        );
    }
}

/// Fire the first loss rule that still applies to a message of `kind`,
/// naming it; rules are consumed as they fire.
fn consume_drop(drops: &mut [Loss], kind: Option<MsgKind>) -> Option<String> {
    for rule in drops.iter_mut() {
        let fired = match rule {
            Loss::Next(remaining) => {
                let applies = *remaining > 0;
                if applies {
                    *remaining -= 1;
                }
                applies.then(|| "next".to_string())
            }
            Loss::Kind(target, remaining) => {
                let applies = *remaining > 0 && Some(*target) == kind;
                if applies {
                    *remaining -= 1;
                }
                applies.then(|| format!("{target:?}"))
            }
        };
        if fired.is_some() {
            return fired;
        }
    }
    None
}

impl Drop for Net {
    fn drop(&mut self) {
        if !updating_traces() {
            return;
        }
        let path = fixture_path(&self.scenario);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &self.trace).unwrap();
    }
}

fn updating_traces() -> bool {
    std::env::var_os("UPDATE_TRACES").is_some()
}

fn fixture_path(scenario: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("fixtures/{scenario}.trace"))
}

fn short(identity: ValidatorIdentity) -> String {
    identity.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn short_version(version: MeshVersion) -> String {
    version.0[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn short_hash(hash: [u8; 32]) -> String {
    hash[..4].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn addr(octet: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([8, 8, 9, octet], port))
}

/// Did `node` verify and apply epoch `epoch` with `peers` tunnels?
pub fn converged(net: &Net, node: usize, epoch: u64, peers: usize) -> bool {
    let ready = net.saw(
        node,
        |e| matches!(e, ReachabilityEvent::MeshReady { epoch: got, .. } if *got == epoch),
    );
    let applied = net.saw(node, |e| {
        matches!(e, ReachabilityEvent::TunnelsApplied { epoch: got, peers: got_peers, .. } if *got == epoch && *got_peers == peers)
    });
    ready && applied
}
