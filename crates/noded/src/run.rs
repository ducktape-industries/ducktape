use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use abi::Outcome;
use abi::Scan;
use abi::role;
use abi::role::validators::{self, Grant, INVITE_NAMESPACE, Invite, Member, Op};
use blob::contract::{BUNDLES, Bundle};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_cryptography::{Digestible as _, Signer as _};
use commonware_p2p::authenticated::lookup::{Oracle, Receiver};
use commonware_runtime::Clock;
use commonware_runtime::Handle;
use commonware_runtime::telemetry::metrics::histogram::Buckets;
use commonware_runtime::telemetry::metrics::{Gauge, Histogram, MetricsExt as _, raw};
use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use consensus::{
    Ancestry, Anchor, Cadence, Chain, EngineMux, Marshal, Membership, Network, Roster, Transport,
    validators_of,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use halo::{Class, Halo, Lane, Seat};
use host::Layer;
use node::{Block, Frame, Node, Sequenced};
use tokio::sync::watch;

use crate::changes::Changes;
use crate::endpoint::Endpoint;
use crate::history::{Durability, History};
use crate::live;
use crate::mesh::{Mesh, Outbound, Tracker};
use crate::pool::{self, Offer, Pooling};
use crate::relay::{self, Relay};
use crate::transport;
use crate::view::{Current, Facts, View, Views};
use crate::workspace::{Descriptor, Founding, Workspace};
use crate::writer::{Command, Hold, Writer};
use crate::{Client, Context, Daemon, Error, Logs, Result, millis};

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Delivery = (Arc<Block>, Exact);
type Seating<E> =
    Membership<E, Outbound<E>, Receiver<PublicKey>, Oracle<PublicKey>, Participant<E>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listen {
    pub p2p: SocketAddr,
    pub http: SocketAddr,
    pub tls: Option<SocketAddr>,
    pub halo: SocketAddr,
    /// The UDP socket desktop clients reach the live relay on over QUIC; without it they take
    /// the websocket alone.
    pub live: Option<SocketAddr>,
}

/// Every socket a node serves on, bound: what `run_on` runs on. `Listen::bind` binds them at
/// their addresses; a caller that binds them itself knows each port before the node starts.
pub struct Listeners {
    pub p2p: std::net::TcpListener,
    pub http: std::net::TcpListener,
    pub tls: Option<std::net::TcpListener>,
    pub halo: std::net::UdpSocket,
    pub live: Option<std::net::UdpSocket>,
}

impl Listen {
    /// Binds every socket, so one in use refuses the start before the node does any work.
    pub fn bind(&self) -> Result<Listeners> {
        let halo = std::net::UdpSocket::bind(self.halo)
            .map_err(|error| Error::Address(format!("--halo {}: {error}", self.halo)))?;
        let live = self
            .live
            .map(|at| {
                std::net::UdpSocket::bind(at)
                    .map_err(|error| Error::Address(format!("--live {at}: {error}")))
            })
            .transpose()?;
        let tcp = std::net::TcpListener::bind;
        Ok(Listeners {
            p2p: tcp(self.p2p)?,
            http: tcp(self.http)?,
            tls: self.tls.map(tcp).transpose()?,
            halo,
            live,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HaloConfig {
    pub egress_bytes_per_sec: Option<u64>,
    pub inbound_streams: NonZeroUsize,
    pub datagrams: NonZeroUsize,
}

impl Default for HaloConfig {
    fn default() -> HaloConfig {
        let queues = halo::Queues::default();
        HaloConfig {
            egress_bytes_per_sec: None,
            inbound_streams: queues.inbound,
            datagrams: queues.datagrams,
        }
    }
}

impl HaloConfig {
    fn queues(&self) -> halo::Queues {
        halo::Queues {
            inbound: self.inbound_streams,
            datagrams: self.datagrams,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadConfig {
    pub blob_backlog: NonZeroUsize,
    pub blob_serves_in_flight: NonZeroUsize,
    pub blob_gatherers: NonZeroUsize,
    pub live_session_inbound: NonZeroUsize,
    pub live_session_outbound: NonZeroUsize,
    pub live_group_queue: NonZeroUsize,
    pub live_peer_queue: NonZeroUsize,
    pub live_lane_backlog: NonZeroUsize,
    pub live_lane_per_sender: NonZeroUsize,
    pub live_linger: Duration,
    pub egress_lane_backlog: NonZeroUsize,
    pub rpc_cpus: Option<Vec<usize>>,
    pub rpc_blocking_threads: Option<NonZeroUsize>,
}

impl Default for LoadConfig {
    fn default() -> LoadConfig {
        let blobs = blob::service::Config::default();
        let live = live::Config::default();
        LoadConfig {
            blob_backlog: blobs.backlog,
            blob_serves_in_flight: blobs.serving,
            blob_gatherers: blobs.gatherers,
            live_session_inbound: live.session_inbound,
            live_session_outbound: live.session_outbound,
            live_group_queue: live.group_queue,
            live_peer_queue: live.peer_queue,
            live_lane_backlog: live.lane_backlog,
            live_lane_per_sender: live.lane_per_sender,
            live_linger: live.linger,
            egress_lane_backlog: live.lane_backlog,
            rpc_cpus: None,
            rpc_blocking_threads: None,
        }
    }
}

impl LoadConfig {
    fn blobs(&self) -> blob::service::Config {
        blob::service::Config {
            backlog: self.blob_backlog,
            serving: self.blob_serves_in_flight,
            gatherers: self.blob_gatherers,
        }
    }

    fn live(&self) -> live::Config {
        live::Config {
            session_inbound: self.live_session_inbound,
            session_outbound: self.live_session_outbound,
            group_queue: self.live_group_queue,
            peer_queue: self.live_peer_queue,
            lane_backlog: self.live_lane_backlog,
            lane_per_sender: self.live_lane_per_sender,
            linger: self.live_linger,
        }
    }
}

pub struct Running<E: Context> {
    pub http: SocketAddr,
    pub p2p: SocketAddr,
    pub daemon: Arc<Daemon<E>>,
    marshal: Marshal,
    mesh: Mesh<E>,
    tasks: Vec<Handle<()>>,
    rpc: crate::server::Rpc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Asked,
    Signalled,
}

impl Stop {
    fn reason(self) -> &'static str {
        match self {
            Stop::Asked => "shutdown",
            Stop::Signalled => "signal",
        }
    }
}

impl<E: Context> Running<E> {
    pub fn rpc(&self) -> Option<tokio::runtime::Handle> {
        self.rpc.handle()
    }

    pub async fn stopped(self, signalled: impl Future<Output = ()>) -> Result<()> {
        let mut down = self.daemon.shutdown.subscribe();
        let stop = tokio::select! {
            _ = down.wait_for(|down| *down) => Stop::Asked,
            () = signalled => Stop::Signalled,
        };
        self.close(stop).await
    }

    async fn close(self, stop: Stop) -> Result<()> {
        let Running {
            daemon,
            marshal,
            mesh,
            tasks,
            rpc,
            ..
        } = self;
        daemon.shutdown.send_replace(true);
        let closed = daemon.writer.close().await;
        for task in &tasks {
            task.abort();
        }
        daemon.hang_up().await;
        drop(rpc);
        marshal.stop().await;
        drop(mesh);
        daemon.activity.close().await;
        let flushed = daemon.history.close();
        let height = closed?;
        flushed?;
        tracing::info!(
            target: "ducktape::node",
            event = "node_stopped",
            reason = stop.reason(),
            height,
            "the node stopped taking blocks and closed its state"
        );
        Ok(())
    }
}

pub fn init<'a, E: Context>(
    context: E,
    workspace: &'a Workspace,
    founding: &'a Path,
    halo: &'a str,
) -> Boxed<'a, Result<()>> {
    Box::pin(found(context, workspace, founding, halo))
}

pub fn join<'a, E: Context>(
    context: E,
    workspace: &'a Workspace,
    source: Client,
    address: Option<Endpoint>,
    halo: String,
    invite: Option<Invite>,
) -> Boxed<'a, Result<()>> {
    Box::pin(adopt(context, workspace, source, address, halo, invite))
}

pub fn enroll<'a, E: Context>(
    mut context: E,
    workspace: &'a Workspace,
    source: Client,
    halo: String,
    invite: Option<Invite>,
) -> Boxed<'a, Result<()>> {
    Box::pin(async move {
        enrolled(&mut context, workspace, &source, None, halo, invite).await?;
        Ok(())
    })
}

pub fn invite(identity: &PrivateKey, grant: Grant) -> Invite {
    let signature = identity.sign(INVITE_NAMESPACE, &grant.preimage());
    Invite {
        issuer: identity.public_key().as_ref().to_vec(),
        grant,
        signature: signature.as_ref().to_vec(),
    }
}

pub fn run<'a, E: Context>(
    context: E,
    workspace: &'a Workspace,
    logs: Logs,
    listen: Listen,
    halo: HaloConfig,
    load: LoadConfig,
    cadence: Cadence,
) -> Boxed<'a, Result<Running<E>>> {
    Box::pin(async move {
        let listeners = listen.bind()?;
        start(context, workspace, logs, listeners, halo, load, cadence).await
    })
}

/// `run` on sockets the caller bound.
pub fn run_on<'a, E: Context>(
    context: E,
    workspace: &'a Workspace,
    logs: Logs,
    listeners: Listeners,
    halo: HaloConfig,
    load: LoadConfig,
    cadence: Cadence,
) -> Boxed<'a, Result<Running<E>>> {
    Box::pin(start(
        context, workspace, logs, listeners, halo, load, cadence,
    ))
}

async fn found<E: Context>(
    mut context: E,
    workspace: &Workspace,
    founding: &Path,
    halo: &str,
) -> Result<()> {
    let (mut founding, base) = Founding::read(founding)?;
    let identity = workspace.identity_or_create(&mut context)?;
    founding.seat_halo(identity.public_key().as_ref(), halo)?;
    let descriptor = founding.descriptor();
    let genesis = founding.genesis(&base)?;
    let (_, block, _) = Node::found(
        context.child("node"),
        &descriptor.network,
        workspace.dir(),
        genesis,
    )
    .await?;
    workspace.write_descriptor(&descriptor)?;
    workspace.write_anchor(&Anchor::Genesis(block))
}

async fn adopt<E: Context>(
    mut context: E,
    workspace: &Workspace,
    source: Client,
    address: Option<Endpoint>,
    halo: String,
    invite: Option<Invite>,
) -> Result<()> {
    let status = enrolled(&mut context, workspace, &source, address, halo, invite).await?;
    let descriptor = Descriptor {
        network: status.network,
        time: status.time,
        block_time_ms: status.block_time_ms,
    };
    let base = source.base().to_owned();
    let joined = statesync::join(
        context.child("join"),
        &descriptor.network,
        workspace.dir(),
        descriptor.id(),
        Duration::from_millis(descriptor.block_time_ms),
        source,
    )
    .await?;
    workspace.write_source(&base)?;
    workspace.write_descriptor(&descriptor)?;
    workspace.write_anchor(&joined.anchor)
}

async fn enrolled<E: Context>(
    context: &mut E,
    workspace: &Workspace,
    source: &Client,
    address: Option<Endpoint>,
    halo: String,
    invite: Option<Invite>,
) -> Result<crate::Status> {
    let halo = crate::workspace::halo_address(&halo)?;
    let status = source.status().await?;
    let identity = workspace.identity_or_create(&mut *context)?;
    let salt = rand_core::Rng::next_u64(&mut *context);
    let enroll = Op::Enroll {
        address: address.as_ref().map(Endpoint::to_string),
        halo,
        invite,
    };
    let frame = Frame::sign(
        &identity,
        status.network.as_bytes(),
        status.anchor(),
        Some(salt),
        status
            .roles
            .bound(role::VALIDATORS)
            .map_err(Error::Refused)?,
        abi::encode(&enroll),
    );
    let (_, receipt) = source.land(&frame).await?;
    if let Outcome::Rejected(refusal) = receipt.outcome {
        return Err(Error::Refused(refusal));
    }
    seated(source, identity.public_key().as_ref()).await?;
    Ok(status)
}

async fn seated(source: &Client, key: &[u8]) -> Result<()> {
    let mut blocks = source.changes(host::RECENT).await?;
    loop {
        let network = source.network().await?;
        let member = network.members.iter().any(|row| row.key == key);
        if member {
            return Ok(());
        }
        blocks.next().await.ok_or(Error::Closed)??;
    }
}

async fn start<E: Context>(
    context: E,
    workspace: &Workspace,
    logs: Logs,
    listeners: Listeners,
    halo: HaloConfig,
    load: LoadConfig,
    cadence: Cadence,
) -> Result<Running<E>> {
    let p2p = listeners.p2p.local_addr()?;
    let udp = listeners.halo.local_addr()?;
    let descriptor = workspace.descriptor()?;
    let identity = workspace.identity()?;
    let anchor = workspace.anchor()?;
    let egress = crate::egress::Station::open(workspace.clone())?;
    let node = Node::open(context.child("node"), &descriptor.network, workspace.dir()).await?;
    let network = Network {
        epoch_length: node.epoch_length()?,
        cadence,
    };
    let history_dir = workspace.dir().join(crate::history::DIR);
    let carried =
        crate::carry::carry_history(context.child("history"), &descriptor.network, &history_dir)
            .await?;
    let history = History::open(&history_dir, Durability::EachBlock)?;
    let seated = recorded_epochs(&node)?;
    let tip = node.tip()?;
    let epoch = network.epoch_after(tip.height);
    let validators = seated.get(&epoch).cloned().ok_or(Error::Corrupt(format!(
        "the state records no validators for epoch {epoch}: it predates validators being \
         recorded apart from members and must be founded again"
    )))?;
    let members = node.epoch_members(epoch)?.ok_or(Error::Corrupt(format!(
        "the state records no members for epoch {epoch}"
    )))?;
    let voters = validators_of(&validators).ok_or(Error::Corrupt(format!(
        "epoch {epoch} seats an undecodable key"
    )))?;

    let halo = Arc::new(
        Halo::start(
            &context.child("halo"),
            identity.clone(),
            halo::Config {
                egress_bytes_per_sec: halo.egress_bytes_per_sec,
                udp: listeners.halo,
                queues: halo.queues(),
            },
        )
        .map_err(|error| Error::Address(format!("--halo {udp}: {error}")))?,
    );
    halo.seat(halo_seats(&node, &network).await?);
    tabled(&halo, halo_lanes(&node).await?, tip.height);
    let blobs = blob::service::Service::start(
        halo.clone(),
        node.host().blobs().clone(),
        bundles(&node)?,
        load.blobs(),
    )?;
    let member_cap = node.member_cap()?;
    let identity_role = node.host().role(role::IDENTITY)?.clone();
    let identity_key = identity.public_key().as_ref().to_vec();
    let (readers, _) = watch::channel(());
    let facts = Facts {
        descriptor: &descriptor,
        network: &network,
        identity: &identity_key,
        readers: &readers,
    };
    let base = Arc::new(View::of(&node, &facts)?);
    let current = Arc::new(Current::new());
    current.views.store(Some(base.clone()));
    let seeding = base.clone();
    let rpc_cpus = match &load.rpc_cpus {
        Some(cpus) => cpus.clone(),
        None => crate::cores::usable()?,
    };
    let indexed = workspace.dir().join(crate::activity::DIR);
    let lag = crate::activity::Lag {
        from: tip.height,
        epoch: network.epoch_length,
    };
    let snapshot_removals = crate::cores::beside_blocks(rpc_cpus.clone(), {
        let dir = workspace.dir().join(crate::snapshot::DIR);
        move || crate::snapshot::Removals::start(dir)
    })??;
    let activity = crate::cores::beside_blocks(rpc_cpus, move || {
        crate::activity::Index::open(&indexed, lag, move || seed_of(&seeding))
    })??;

    // the live door opens on its socket here, beside halo's, before consensus does any work
    let (shutdown, _) = watch::channel(false);
    let door = listeners
        .live
        .map(|socket| {
            let at = socket.local_addr()?;
            live::door(&identity, socket)
                .map_err(|error| Error::Address(format!("--live {at}: {error}")))
        })
        .transpose()?;
    let live = live::Relay::start(
        context.child("live"),
        live::Network {
            id: live::Network::chain_id(
                &descriptor.network,
                &descriptor.genesis_block().digest().0,
            ),
            genesis: descriptor.genesis_block().digest().0,
            identity: identity_role,
        },
        Arc::new(Views::new(current.clone(), context.child("views"))),
        halo.clone(),
        door,
        load.live(),
        shutdown.subscribe(),
    )?;

    let (mesh, mut tracker, marshal_lanes, engine_channels, (relay_sender, relay_receiver)) =
        Mesh::start(
            context.child("mesh"),
            identity.clone(),
            &descriptor.id(),
            listeners.p2p,
            member_cap,
            cadence,
        )?;
    tracker.track(epoch, &members);
    let roster = Roster::new(descriptor.id(), Some(identity.clone()));
    for (epoch, validators) in &seated {
        let validators = validators_of(validators).ok_or(Error::Corrupt(format!(
            "epoch {epoch} seats an undecodable key"
        )))?;
        roster.seat(*epoch, validators);
    }

    let (tips, _) = watch::channel(tip.height);
    let relay = Arc::new(Relay::new(
        identity.public_key(),
        relay_sender,
        voters.as_ref(),
    ));
    let pooling = pool::start(
        context.child("pool"),
        node.verified(),
        base,
        current.clone(),
        relay.clone(),
        blobs.posts(),
        tips.subscribe(),
    )?;
    let (inbox, deliveries) = mpsc::unbounded();
    let (finalized, known) = watch::channel(tip.height);
    let chain = Participant {
        pool: pooling.clone(),
        inbox,
        finalized: Arc::new(finalized),
    };
    let marshal = Marshal::start(
        context.child("marshal"),
        &descriptor.network,
        &network,
        roster.clone(),
        anchor,
        Transport {
            me: identity.public_key(),
            provider: mesh.oracle(),
            blocker: mesh.oracle(),
            lanes: marshal_lanes,
        },
        chain.clone(),
    )
    .await?;
    let mux = EngineMux::start(context.child("lanes"), engine_channels, mesh.oracle());
    let mut membership = Membership::new(
        context.child("membership"),
        descriptor.network.clone(),
        network.clone(),
        roster,
        mux,
        &marshal,
        chain,
    );
    let standing = membership.seat(tip, &validators).await?;
    tracing::info!(
        target: "ducktape::node",
        event = "node_started",
        network = descriptor.network,
        epoch,
        ?standing,
        block_time_ms = cadence.block_time_ms(),
        leader_timeout_ms = cadence.leader_timeout().as_millis() as u64,
        certification_timeout_ms = cadence.certification_timeout().as_millis() as u64,
        timeout_retry_ms = cadence.timeout_retry().as_millis() as u64,
        link_timeout_ms = crate::mesh::link_timeout(cadence).as_millis() as u64,
        ping_frequency_ms = crate::mesh::ping_frequency(cadence).as_millis() as u64,
        "the node is up"
    );

    let writing = context.child("writer");
    let tally = Tally::registered(&writing);
    let (writer, commands) = Writer::new(tally.queued.clone());
    let (changes, fanout) = Changes::new(network.clone(), tip.height);
    let (answered, answers) = tokio::sync::mpsc::unbounded_channel();
    let daemon = Arc::new(Daemon {
        context: context.child("daemon"),
        current,
        readers,
        writer,
        pool: pooling,
        changes,
        descriptor: descriptor.clone(),
        network,
        identity: identity_key,
        anchors: marshal.mailbox().clone(),
        holds: statesync::Holds::default(),
        holds_ended: Default::default(),
        snapshot: snapshot_removals.served(),
        history,
        recent: crate::blocks::Recent::new(),
        activity,
        logs,
        shutdown,
        tips,
        votes: membership.votes().clone(),
        relay,
        blobs,
        halo,
        live,
        egress,
        answered,
        key: identity.clone(),
        mesh: transport::loopback(p2p),
    });
    let founding = tip.height == 0;
    if let Anchor::Genesis(genesis) = marshal.anchor()
        && founding
    {
        daemon.record_unrecorded(&node, genesis).await?;
    }
    let applied_unsummarized = carried
        .unsummarized
        .into_iter()
        .filter(|height| *height <= tip.height);
    let archived = marshal.mailbox().clone();
    for height in applied_unsummarized {
        let asked = commonware_consensus::types::Height::new(height);
        let block = archived.get_block(asked).await.ok_or_else(|| {
            Error::Corrupt(format!(
                "block {height} is archived and marshal does not read it"
            ))
        })?;
        daemon.record_unrecorded(&node, &block).await?;
    }

    let pump = context.child("pump").spawn({
        let writing = Writing {
            daemon: daemon.clone(),
            node,
            commands,
            membership,
            tracker,
            tally,
            snapshots: crate::snapshot::Snapshots::new(
                workspace.dir().join(crate::snapshot::DIR),
                workspace.runtime_dir(),
                crate::snapshot::reflink,
                snapshot_removals,
                known,
                daemon.network.epoch_length,
            ),
        };
        move |_| write(writing, deliveries)
    });
    let relayed = context.child("relay").spawn({
        let daemon = daemon.clone();
        move |_| relay::serve(daemon, relay_receiver)
    });
    let http = listeners.http.local_addr()?;
    let tls = match listeners.tls {
        Some(socket) => Some((transport::served(socket)?, transport::acceptor()?)),
        None => None,
    };
    let rpc = crate::server::Rpc::start(
        daemon.clone(),
        listeners.http,
        fanout,
        answers,
        load.rpc_cpus,
        load.rpc_blocking_threads,
    )?;
    if let (Some(source), Some(handle)) = (workspace.source()?, rpc.handle()) {
        handle.spawn(crate::fill::fill(daemon.clone(), Client::new(source)));
    }
    if let Some(handle) = rpc.handle() {
        crate::egress::lane::spawn(daemon.clone(), handle, load.egress_lane_backlog)?;
    }
    let egressing = crate::egress::spawn(daemon.clone());
    let mut tasks = vec![pump, relayed, egressing];
    if let Some((listener, acceptor)) = tls {
        let mesh = transport::loopback(p2p);
        tasks.push(
            context
                .child("tls")
                .spawn(move |_| transport::inbound_tls(listener, acceptor, mesh)),
        );
    }
    workspace.write_http(http)?;
    Ok(Running {
        http,
        p2p,
        daemon,
        marshal,
        mesh,
        tasks,
        rpc,
    })
}

fn seed_of<E: Context>(view: &View<E>) -> Result<crate::activity::Seed> {
    let rows = view
        .source()
        .view(Layer::Confirmed)
        .scan(crate::activity::PROGRAM, &Scan::prefix(b""))
        .map_err(host::Error::from)?;
    Ok(crate::activity::Seed {
        height: view.tip().height,
        rows,
    })
}

fn bundles<E: Context>(node: &Node<E>) -> Result<Vec<(u64, Bundle)>> {
    let role = node.host().role(role::BLOBS)?;
    let entries = node
        .view(Layer::Confirmed)
        .scan(role, &Scan::prefix(BUNDLES))
        .map_err(host::Error::from)?;
    blob::contract::recorded(&entries).map_err(|unreadable| {
        Error::Corrupt(format!(
            "bundle {} does not decode: {}",
            unreadable.bundle, unreadable.refusal.sentence
        ))
    })
}

async fn halo_seats<E: Context>(node: &Node<E>, network: &Network) -> Result<Vec<Seat>> {
    let epoch = network.epoch_after(node.tip()?.height);
    let validators = node.epoch_validators(epoch)?.ok_or(Error::Corrupt(format!(
        "the state records no validators for epoch {epoch}"
    )))?;
    let role = node.host().role(role::VALIDATORS)?.clone();
    let asked = abi::encode(&validators::Query::Members);
    let answer = node
        .query(Layer::Confirmed, abi::Origin::System, &role, asked)
        .await?
        .map_err(|refusal| {
            Error::Corrupt(format!("the validators program refused Members: {refusal}"))
        })?;
    let validators::Reply::Members(members) = abi::decode(&answer).map_err(|refusal| {
        Error::Corrupt(format!(
            "the Members answer does not decode: {}",
            refusal.sentence
        ))
    })?
    else {
        return Err(Error::Corrupt(
            "the validators program answered Members with another reply".into(),
        ));
    };
    Ok(seats(&members, &validators))
}

/// The lanes the halo role lists, at the confirmed tip.
async fn halo_lanes<E: Context>(node: &Node<E>) -> Result<Vec<Lane>> {
    let role = node.host().role(role::HALO)?.clone();
    let asked = abi::encode(&abi::role::halo::Query::Lanes);
    let answer = node
        .query(Layer::Confirmed, abi::Origin::System, &role, asked)
        .await?
        .map_err(|refusal| Error::Corrupt(format!("the halo program refused Lanes: {refusal}")))?;
    let abi::role::halo::Reply::Lanes(lanes) = abi::decode(&answer).map_err(|refusal| {
        Error::Corrupt(format!(
            "the Lanes answer does not decode: {}",
            refusal.sentence
        ))
    })?
    else {
        return Err(Error::Corrupt(
            "the halo program answered Lanes with another reply".into(),
        ));
    };
    Ok(lanes)
}

/// Whether a block's writes change what `program` answers: it wrote the program's state, or the
/// host swapped its code (a write under `$programs` at the program's id).
fn rewrites(writes: &state::Writes, program: &str) -> bool {
    let state = writes
        .programs
        .get(program)
        .is_some_and(|written| !written.is_empty());
    let code = writes
        .programs
        .get(host::PROGRAMS)
        .is_some_and(|written| written.contains_key(program.as_bytes()));
    state || code
}

async fn retable_halo<E: Context>(daemon: &Daemon<E>, node: &Node<E>, height: u64) {
    match halo_lanes(node).await {
        Ok(lanes) => tabled(&daemon.halo, lanes, height),
        Err(error) => tracing::warn!(
            target: "ducktape::halo",
            event = "halo_untabled",
            height,
            %error,
            reason = "lanes_unread",
            "halo keeps its lane table: the node could not read the halo program's lanes"
        ),
    }
}

/// Has halo serve `lanes`, the halo role's table at `height`, from now on, and says so.
fn tabled(halo: &Halo, lanes: Vec<Lane>, height: u64) {
    tracing::info!(
        target: "ducktape::halo",
        event = "halo_retabled",
        height,
        lanes = listing(&lanes),
        "halo serves the halo role's lane table"
    );
    halo.plane().lanes(lanes);
}

/// The lane table as one log field, in id order: `name#id cap`, then `paused` or `served`, then
/// `pinned` for a pinned lane.
fn listing(lanes: &[Lane]) -> String {
    let lane = |lane: &Lane| {
        let served = match lane.enabled {
            true => "served",
            false => "paused",
        };
        let pinned = match lane.pinned {
            true => " pinned",
            false => "",
        };
        let cap = match lane.cap {
            Class::Critical => "critical",
            Class::Member => "member",
            Class::Client => "client",
        };
        format!("{}#{} {cap} {served}{pinned}", lane.name, lane.id)
    };
    lanes.iter().map(lane).collect::<Vec<_>>().join(", ")
}

async fn reseat_halo<E: Context>(daemon: &Daemon<E>, node: &Node<E>, height: u64) {
    match halo_seats(node, &daemon.network).await {
        Ok(seats) => daemon.halo.seat(seats),
        Err(error) => tracing::warn!(
            target: "ducktape::halo",
            event = "halo_unseated",
            height,
            %error,
            reason = "members_unread",
            "halo keeps its seating: the node could not read the validators program's members"
        ),
    }
}

pub fn seats(members: &[Member], validators: &[Vec<u8>]) -> Vec<Seat> {
    members
        .iter()
        .filter_map(|member| {
            let key = PublicKey::decode(member.key.as_slice()).ok()?;
            let validator = validators.contains(&member.key);
            Some(Seat {
                key,
                halo: member.halo.clone(),
                validator,
            })
        })
        .collect()
}

/// Every recorded epoch's validator keys: the sets the roster verifies
/// certificates against.
fn recorded_epochs<E: Context>(
    node: &Node<E>,
) -> Result<std::collections::BTreeMap<u64, Vec<Vec<u8>>>> {
    let mut seated = std::collections::BTreeMap::new();
    let mut epoch = 0;
    while let Some(validators) = node.epoch_validators(epoch)? {
        seated.insert(epoch, validators);
        epoch += 1;
    }
    Ok(seated)
}

pub struct Participant<E: Context> {
    pool: Pooling<E>,
    inbox: mpsc::UnboundedSender<Delivery>,
    finalized: Arc<watch::Sender<u64>>,
}

impl<E: Context> Clone for Participant<E> {
    fn clone(&self) -> Participant<E> {
        Participant {
            pool: self.pool.clone(),
            inbox: self.inbox.clone(),
            finalized: self.finalized.clone(),
        }
    }
}

impl<E: Context> Chain for Participant<E> {
    async fn propose(
        &mut self,
        parent: Arc<Block>,
        ancestors: impl Ancestry<Block>,
        time: u64,
    ) -> Option<Block> {
        let applied = self.pool.applied();
        let unapplied = consensus::unapplied(parent.clone(), ancestors, applied).await;
        self.pool.build(parent.tip(), unapplied, time).await
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.inbox.unbounded_send((block, ack));
    }

    fn finalized(&mut self, height: u64) {
        self.finalized.send_replace(height);
    }
}

struct Writing<E: Context> {
    daemon: Arc<Daemon<E>>,
    node: Node<E>,
    commands: mpsc::UnboundedReceiver<Command>,
    membership: Seating<E>,
    tracker: Tracker,
    tally: Tally,
    snapshots: crate::snapshot::Snapshots,
}

struct Tally {
    queued: Gauge,
    applying: Histogram,
    commands: u64,
    commanding: Duration,
}

impl Tally {
    fn registered<E: Context>(context: &E) -> Tally {
        Tally {
            queued: context.register(
                "queued_commands",
                "commands sent to the writer and not yet taken",
                raw::Gauge::default(),
            ),
            applying: context.histogram(
                "block_apply_seconds",
                "time from the writer taking a block to finishing with it",
                Buckets::NETWORK,
            ),
            commands: 0,
            commanding: Duration::ZERO,
        }
    }
}

struct Lap<'a, C> {
    clock: &'a C,
    at: SystemTime,
}

impl<'a, C: Clock> Lap<'a, C> {
    fn start(clock: &'a C) -> Lap<'a, C> {
        Lap {
            clock,
            at: clock.current(),
        }
    }

    fn lap(&mut self) -> Duration {
        let now = self.clock.current();
        let spent = now.duration_since(self.at).unwrap_or_default();
        self.at = now;
        spent
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Slowest {
    spent: Duration,
    program: String,
    position: u32,
    tx: String,
}

fn slowest_of(
    spent: Option<host::Slowest>,
    receipts: &[abi::Receipt],
    carried: &[node::Carried],
) -> Option<Slowest> {
    let spent = spent?;
    let at = spent.position as usize;
    Some(Slowest {
        spent: spent.spent,
        program: receipts.get(at)?.program.clone(),
        position: spent.position,
        tx: hex::encode(carried.get(at)?.id),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Phases {
    total: Duration,
    behind: Duration,
    applying: Duration,
    executing: Duration,
    committing: Duration,
    publishing: Duration,
    recording: Duration,
    proposing: Duration,
    keeping: Duration,
    receipting: Duration,
    seating: Duration,
    snapshotting: Duration,
    slowest: Option<Slowest>,
}

impl Phases {
    fn slow(&self, block_time_ms: u64) -> bool {
        self.total > Duration::from_millis(block_time_ms)
    }

    fn verifying(&self) -> Duration {
        self.applying
            .saturating_sub(self.executing)
            .saturating_sub(self.committing)
    }
}

fn ms(spent: Duration) -> u64 {
    spent.as_millis() as u64
}

fn reported(phases: &Phases, tally: &Tally, height: u64, block_time_ms: u64) {
    tally.applying.observe(phases.total.as_secs_f64());
    if !phases.slow(block_time_ms) {
        return;
    }
    tracing::warn!(
        target: "ducktape::node",
        event = "apply_slow",
        height,
        total_ms = ms(phases.total),
        block_time_ms,
        behind_ms = ms(phases.behind),
        queued = tally.queued.get(),
        commands = tally.commands,
        commands_ms = ms(tally.commanding),
        executing_ms = ms(phases.executing),
        committing_ms = ms(phases.committing),
        verifying_ms = ms(phases.verifying()),
        publishing_ms = ms(phases.publishing),
        recording_ms = ms(phases.recording),
        proposer_ms = ms(phases.proposing),
        history_ms = ms(phases.keeping),
        receipting_ms = ms(phases.receipting),
        seating_ms = ms(phases.seating),
        snapshot_ms = ms(phases.snapshotting),
        slowest_ms = phases.slowest.as_ref().map(|slowest| ms(slowest.spent)),
        slowest_program = phases.slowest.as_ref().map(|slowest| slowest.program.as_str()),
        slowest_position = phases.slowest.as_ref().map(|slowest| slowest.position),
        slowest_tx = phases.slowest.as_ref().map(|slowest| slowest.tx.as_str()),
        reason = "block_time_exceeded",
        "the writer took longer than a block time to apply a block"
    );
}

enum Event<D = Delivery, C = Command> {
    Delivered(D),
    Unfed,
    Asked(Option<C>),
}

type Closer = futures::channel::oneshot::Sender<Result<u64>>;

enum Flow {
    Writing,
    Closing(Closer),
    Unasked,
}

enum Held {
    Sequenced(Sequenced),
    Closing(Closer),
    Unasked,
}

async fn write<E: Context>(mut writing: Writing<E>, deliveries: mpsc::UnboundedReceiver<Delivery>) {
    let mut deliveries = Some(deliveries);
    loop {
        let event = next(&mut deliveries, &mut writing.commands).await;
        let flow = match event {
            Event::Delivered(delivery) => delivered(&mut writing, &mut deliveries, delivery).await,
            Event::Unfed => unfed(&mut deliveries),
            Event::Asked(command) => commanded(&mut writing, command).await,
        };
        match flow {
            Flow::Writing => {}
            Flow::Closing(closer) => return closing(writing, closer).await,
            Flow::Unasked => return,
        }
    }
}

async fn next<D, C>(
    deliveries: &mut Option<mpsc::UnboundedReceiver<D>>,
    commands: &mut mpsc::UnboundedReceiver<C>,
) -> Event<D, C> {
    let Some(fed) = deliveries else {
        return Event::Asked(commands.next().await);
    };
    tokio::select! {
        biased;
        delivery = fed.next() => delivery.map_or(Event::Unfed, Event::Delivered),
        command = commands.next() => Event::Asked(command),
    }
}

fn unfed(deliveries: &mut Option<mpsc::UnboundedReceiver<Delivery>>) -> Flow {
    *deliveries = None;
    Flow::Writing
}

async fn delivered<E: Context>(
    writing: &mut Writing<E>,
    deliveries: &mut Option<mpsc::UnboundedReceiver<Delivery>>,
    (block, ack): Delivery,
) -> Flow {
    let daemon = writing.daemon.clone();
    let mut whole = Lap::start(&daemon.context);
    let mut phases = Phases {
        behind: Duration::from_millis(millis(&daemon.context).saturating_sub(block.time)),
        ..Phases::default()
    };
    match applied(writing, &block, &mut phases).await {
        Ok(Flow::Writing) => {
            ack.acknowledge();
            phases.total = whole.lap();
            let block_time_ms = daemon.network.cadence.block_time_ms();
            reported(&phases, &writing.tally, block.height, block_time_ms);
            writing.tally.commands = 0;
            writing.tally.commanding = Duration::ZERO;
            Flow::Writing
        }
        Ok(Flow::Closing(closer)) => Flow::Closing(closer),
        Ok(Flow::Unasked) => Flow::Unasked,
        Err(error) => {
            tracing::error!(
                target: "ducktape::node",
                event = "apply_failed",
                height = block.height,
                %error,
                "the node cannot apply a finalized block and stops"
            );
            writing.daemon.shutdown.send_replace(true);
            *deliveries = None;
            Flow::Writing
        }
    }
}

async fn commanded<E: Context>(writing: &mut Writing<E>, command: Option<Command>) -> Flow {
    let clock = writing.daemon.clone();
    asked(&mut writing.tally, &clock.context, command).await
}

async fn asked<E: Context>(tally: &mut Tally, clock: &E, command: Option<Command>) -> Flow {
    let Some(command) = command else {
        return Flow::Unasked;
    };
    tally.queued.dec();
    let mut lap = Lap::start(clock);
    let flow = match command {
        Command::Hold(hold) => held(hold).await,
        Command::Close(closer) => Flow::Closing(closer),
    };
    tally.commands += 1;
    tally.commanding += lap.lap();
    flow
}

async fn held(hold: Hold) -> Flow {
    let _ = hold.holding.send(());
    let _ = hold.released.await;
    Flow::Writing
}

async fn closing<E: Context>(writing: Writing<E>, closer: Closer) {
    let Writing {
        daemon,
        mut node,
        commands,
        membership,
        tracker,
        ..
    } = writing;
    daemon.hang_up().await;
    daemon.current.withdraw();
    daemon.pool.close().await;
    drop(commands);
    membership.stop().await;
    drop(tracker);
    let synced = node.close().await;
    let closed = synced
        .map_err(Error::from)
        .and_then(|()| Ok(node.tip()?.height));
    daemon.readers.closed().await;
    drop(node);
    let _ = closer.send(closed);
}

fn published<E: Context>(node: &Node<E>, daemon: &Daemon<E>) {
    match View::of(node, &daemon.facts()) {
        Ok(view) => {
            daemon.current.views.store(Some(Arc::new(view)));
        }
        Err(error) => tracing::warn!(
            target: "ducktape::node",
            event = "view_unpublished",
            %error,
            reason = "view_unread",
            "the node could not pin a view of its state; reads see the last one"
        ),
    }
}

async fn holding<E: Context>(writing: &mut Writing<E>, block: &Block) -> Result<Held> {
    loop {
        let applied = writing.node.apply(block).await;
        let Err(node::Error::Host(host::Error::BlobUnavailable(id))) = applied else {
            return Ok(Held::Sequenced(applied?));
        };
        tracing::warn!(
            target: "ducktape::node",
            event = "blob_unavailable",
            height = block.height,
            ?id,
            reason = "blob_missing",
            "the block reads a blob this node lacks; the node fetches it and applies again"
        );
        let daemon = writing.daemon.clone();
        let fetched = daemon.blobs.fetch(id, halo::Class::Critical);
        futures::pin_mut!(fetched);
        loop {
            let command = tokio::select! {
                () = &mut fetched => break,
                command = writing.commands.next() => command,
            };
            match asked(&mut writing.tally, &daemon.context, command).await {
                Flow::Writing => {}
                Flow::Closing(closer) => return Ok(Held::Closing(closer)),
                Flow::Unasked => return Ok(Held::Unasked),
            }
        }
    }
}

fn receipts_for<E: Context>(
    daemon: &Daemon<E>,
    node: &Node<E>,
    height: u64,
    due: Vec<blob::service::Due>,
    reseated: bool,
) -> Result<()> {
    let role = node.host().role(role::BLOBS)?.clone();
    for due in due {
        let fresh = Frame::sign(
            &daemon.key,
            &daemon.descriptor.id(),
            node.tip()?,
            Some(height),
            &role,
            abi::encode(&blob::contract::Op::Receipt { bundle: due.bundle }),
        )
        .encode();
        daemon.pool.receipt(pool::Receipt {
            bundle: due.bundle,
            posted: due.posted,
            fresh,
            reseated,
        });
    }
    Ok(())
}

async fn applied<E: Context>(
    writing: &mut Writing<E>,
    block: &Block,
    phases: &mut Phases,
) -> Result<Flow> {
    let clock = writing.daemon.clone();
    let mut lap = Lap::start(&clock.context);
    let sequenced = match holding(writing, block).await? {
        Held::Sequenced(sequenced) => sequenced,
        Held::Closing(closer) => return Ok(Flow::Closing(closer)),
        Held::Unasked => return Ok(Flow::Unasked),
    };
    phases.applying = lap.lap();
    let Writing {
        daemon,
        node,
        membership,
        tracker,
        snapshots,
        ..
    } = writing;
    let daemon: &Daemon<E> = daemon;
    published(node, daemon);
    daemon.votes.applied(block.digest(), block.height);
    daemon.tips.send_replace(block.height);
    phases.publishing = lap.lap();
    let closes = daemon.network.closes_an_epoch(block.height);
    let replayed = matches!(sequenced, Sequenced::Replayed);
    let opens_an_epoch = !replayed && closes;
    let members_written = match sequenced {
        Sequenced::Applied {
            mut applied,
            carried,
        } => {
            let ran = crate::blocks::ran(block, &carried)?;
            let receipts = std::mem::take(&mut applied.submissions);
            let spent = node.host().spent();
            phases.executing = spent.executing;
            phases.committing = spent.committing;
            phases.slowest = slowest_of(spent.slowest, &receipts, &carried);
            let records = std::mem::take(&mut applied.records);
            let refused = crate::activity::refused(&records, &receipts);
            let recorded = daemon.record(node, block, ran, Some(receipts)).await?;
            phases.proposing = recorded.proposing;
            phases.keeping = recorded.keeping;
            daemon.activity.offer(crate::activity::Block {
                height: block.height,
                time: block.time,
                records,
                refused,
            });
            phases.recording = lap.lap();
            daemon.publish(&applied);
            let validators = node.host().role(role::VALIDATORS)?;
            let members_written = applied
                .writes
                .programs
                .get(validators)
                .is_some_and(|writes| !writes.is_empty());
            let halo = node.host().role(role::HALO)?;
            let lanes_written = rewrites(&applied.writes, halo);
            if lanes_written {
                retable_halo(daemon, node, block.height).await;
            }
            let blobs = node.host().role(role::BLOBS)?;
            let writes = applied.writes.programs.get(blobs).cloned();
            let due = daemon
                .blobs
                .applied(block.height, &writes.unwrap_or_default());
            let reseated = members_written || closes;
            receipts_for(daemon, node, block.height, due, reseated)?;
            phases.receipting = lap.lap();
            members_written
        }
        Sequenced::Replayed => {
            daemon.record_unrecorded(node, block).await?;
            phases.recording = lap.lap();
            false
        }
    };
    let reseats = members_written || closes;
    if reseats {
        reseat_halo(daemon, node, block.height).await;
    }
    if !opens_an_epoch {
        daemon.pool.offer(Offer::Due);
        phases.seating = lap.lap();
        return Ok(Flow::Writing);
    }
    let epoch = daemon.network.epoch_after(block.height);
    let unrecorded = || Error::Corrupt(format!("the boundary block records no epoch {epoch}"));
    let members = node.epoch_members(epoch)?.ok_or_else(unrecorded)?;
    let validators = node.epoch_validators(epoch)?.ok_or_else(unrecorded)?;
    let seated = validators_of(&validators).ok_or(Error::Corrupt(format!(
        "epoch {epoch} seats an undecodable key"
    )))?;
    let reseated = daemon.relay.seat(seated.as_ref());
    let offer = match reseated {
        true => Offer::Every,
        false => Offer::Due,
    };
    daemon.pool.offer(offer);
    // The mesh admits every member, so a resident follows the chain; only
    // the validators vote and propose.
    tracker.track(epoch, &members);
    membership.seat(block.tip(), &validators).await?;
    phases.seating = lap.lap();
    let follows = !validators.contains(&daemon.identity);
    if follows {
        snapshots.take(node, block.height);
        phases.snapshotting = lap.lap();
    }
    Ok(Flow::Writing)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use futures::channel::mpsc;

    use abi::Outcome;

    use super::{Event, Phases, Slowest, listing, next, rewrites, slowest_of};

    #[test]
    fn a_program_is_rewritten_by_its_own_state_or_a_swap_of_its_code() {
        let writes = |program: &str, key: &[u8]| state::Writes {
            programs: BTreeMap::from([(
                program.to_owned(),
                BTreeMap::from([(key.to_vec(), Some(vec![1]))]),
            )]),
            local: Default::default(),
        };
        assert!(rewrites(&writes("halo", b"l/x"), "halo"));
        assert!(rewrites(&writes(host::PROGRAMS, b"halo"), "halo"));
        assert!(!rewrites(&writes(host::PROGRAMS, b"blobs"), "halo"));
        assert!(!rewrites(&writes("blobs", b"halo"), "halo"));
        assert!(!rewrites(&state::Writes::default(), "halo"));
    }

    #[tokio::test]
    async fn a_waiting_block_goes_before_a_waiting_command() {
        let (blocks, fed) = mpsc::unbounded();
        let (commands, mut asked) = mpsc::unbounded();
        let turns = 16;
        for turn in 0..turns {
            commands.unbounded_send(turn).unwrap();
            blocks.unbounded_send(turn).unwrap();
        }
        let mut deliveries = Some(fed);
        for turn in 0..turns {
            let event = next(&mut deliveries, &mut asked).await;
            assert!(matches!(event, Event::Delivered(block) if block == turn));
        }
        let event = next(&mut deliveries, &mut asked).await;
        assert!(matches!(event, Event::Asked(Some(0))));
    }

    #[test]
    fn a_block_is_slow_past_one_block_time_and_its_verifying_is_what_the_host_did_not_spend() {
        let phases = Phases {
            total: Duration::from_millis(1001),
            applying: Duration::from_millis(900),
            executing: Duration::from_millis(200),
            committing: Duration::from_millis(300),
            ..Phases::default()
        };
        assert!(phases.slow(1000));
        assert!(!phases.slow(1001));
        assert_eq!(phases.verifying(), Duration::from_millis(400));
        let replayed = Phases {
            applying: Duration::from_millis(5),
            executing: Duration::from_millis(9),
            ..Phases::default()
        };
        assert_eq!(replayed.verifying(), Duration::ZERO);
    }

    #[test]
    fn the_slowest_frame_is_named_by_its_receipt_and_its_carried_id() {
        let receipt = |program: &str| abi::Receipt {
            program: program.into(),
            outcome: Outcome::Applied { output: Vec::new() },
            events: Vec::new(),
            nested: Vec::new(),
        };
        let receipts = [receipt("chat"), receipt("probe")];
        let carried = [
            node::Carried {
                frame: 0,
                id: [1; 32],
            },
            node::Carried {
                frame: 2,
                id: [2; 32],
            },
        ];
        let slowest = host::Slowest {
            position: 1,
            spent: Duration::from_millis(7),
        };
        assert_eq!(
            slowest_of(Some(slowest), &receipts, &carried),
            Some(Slowest {
                spent: Duration::from_millis(7),
                program: "probe".into(),
                position: 1,
                tx: hex::encode([2; 32]),
            })
        );
        assert_eq!(slowest_of(None, &receipts, &carried), None);
    }

    #[test]
    fn the_table_reads_as_one_line() {
        let lane = |name: &str, id, enabled, pinned| halo::Lane {
            name: name.into(),
            id,
            cap: halo::Class::Critical,
            enabled,
            pinned,
            since: 0,
        };
        let table = [
            lane("a/blobs", 0, true, true),
            lane("a/live", 1, false, false),
        ];
        assert_eq!(
            listing(&table),
            "a/blobs#0 critical served pinned, a/live#1 critical paused"
        );
    }
}
