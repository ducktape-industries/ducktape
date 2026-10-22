use std::future::Future;
use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use abi::Outcome;
use abi::Scan;
use abi::role;
use abi::role::validators::{self, Grant, INVITE_NAMESPACE, Invite, Member, Op};
use blob::contract::{BUNDLES, Bundle};
use blob::service::Fetched;
use commonware_codec::{DecodeExt as _, Encode as _};
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
    Ancestry, Anchor, Cadence, Chain, EngineMux, Marshal, MarshalMailbox, Membership, Network,
    Roster, Transport, validators_of,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use halo::{Class, Halo, Lane, Seat};
use host::Layer;
use node::{Block, Frame, Node, Sequenced};
use statesync::Anchors as _;
use tokio::sync::watch;

use crate::changes::Changes;
use crate::endpoint::{Endpoint, Scheme};
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
pub(crate) type Delivery = (Arc<Block>, Exact);
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
    pub forward_lane_backlog: NonZeroUsize,
    pub rpc_cpus: Option<Vec<usize>>,
    pub rpc_blocking_threads: Option<NonZeroUsize>,
    pub query_fuel: Option<NonZeroU64>,
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
            forward_lane_backlog: live.lane_backlog,
            rpc_cpus: None,
            rpc_blocking_threads: None,
            query_fuel: None,
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
    marshal: Option<Marshal>,
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
        if let Some(marshal) = marshal {
            marshal.stop().await;
        }
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
    workspace.write_anchor(&Anchor::Genesis(block))?;
    workspace.write_stamp(&crate::workspace::Stamp {
        contract: crate::wire::NODE_CONTRACT,
        from: 1,
    })
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
    workspace.write_anchor(&joined.anchor)?;
    workspace.write_stamp(&crate::workspace::Stamp {
        contract: crate::wire::NODE_CONTRACT,
        from: joined.node.tip()?.height + 1,
    })
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

async fn opened<E: Context>(
    context: &E,
    workspace: &Workspace,
    descriptor: &Descriptor,
) -> Result<Node<E>> {
    let opened = Node::open(context.child("node"), &descriptor.network, workspace.dir()).await;
    let Err(node::Error::Host(host::Error::Unfounded)) = opened else {
        return Ok(opened?);
    };
    let Some(source) = workspace.source()? else {
        return Err(node::Error::Host(host::Error::Unfounded).into());
    };
    let joined = Box::pin(statesync::join(
        context.child("rejoin"),
        &descriptor.network,
        workspace.dir(),
        descriptor.id(),
        Duration::from_millis(descriptor.block_time_ms),
        Client::new(source),
    ))
    .await?;
    workspace.write_anchor(&joined.anchor)?;
    tracing::warn!(
        target: "ducktape::node",
        event = "state_rejoined",
        height = joined.node.tip()?.height,
        reason = "state_unmarked",
        "the state records no height, as a stop while the node re-adopted it leaves it; the node joined again from its source"
    );
    Ok(joined.node)
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
    let egress = crate::egress::Station::open(workspace.clone())?;
    let mut node = opened(&context, workspace, &descriptor).await?;
    node.meter_queries(load.query_fuel.map(NonZeroU64::get));
    crate::roll::admits(workspace.stamp()?, node.tip()?.height)?;
    let solo = node.host().parent().is_some();
    if solo {
        loopback_only(&listeners)?;
    }
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
    let root = node.host().root()?;
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
                udp: listeners.halo.into(),
                reach: halo::Https::shared(),
                queues: halo.queues(),
            },
        )
        .map_err(|error| Error::Address(format!("--halo {udp}: {error}")))?,
    );
    let seats = match solo {
        true => Vec::new(),
        false => halo_seats(&node, &network).await?,
    };
    halo.seat(seats);
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
    let base = Arc::new(View::of(&node, &facts, None)?);
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
    if !solo {
        tracker.track(epoch, &members);
    }
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
        inbox: inbox.clone(),
        finalized: Arc::new(finalized),
    };
    let steered = match solo {
        true => None,
        false => {
            let marshal = Marshal::start(
                context.child("marshal"),
                &descriptor.network,
                &network,
                roster.clone(),
                workspace.anchor()?,
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
            let standing = membership.seat(tip, root, &validators).await?;
            Some((marshal, membership, standing))
        }
    };
    let standing = steered.as_ref().map(|(_, _, standing)| standing);
    tracing::info!(
        target: "ducktape::node",
        event = "node_started",
        network = descriptor.network,
        epoch,
        solo,
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
    let (changes, fanout) = Changes::new(network.clone());
    let (answered, answers) = tokio::sync::mpsc::unbounded_channel();
    let (queried, queries) = tokio::sync::mpsc::unbounded_channel();
    let halo_self = watch::Sender::new(crate::halo_self::first(&halo));
    let forward = crate::forward::Forwarding::bind(
        &halo,
        load.forward_lane_backlog,
        workspace.forward_offers(),
    )?;
    let block_time = Duration::from_millis(descriptor.block_time_ms);
    let (solo_handle, driving) = match solo {
        true => {
            let (handle, driving) = crate::fork::solo(pooling.clone(), inbox, tip, block_time);
            (Some(handle), Some(driving))
        }
        false => (None, None),
    };
    let crosspond = crate::crosspond::Closes::load(history.clone())?;
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
        holds: statesync::Holds::default(),
        holds_ended: Default::default(),
        snapshot: snapshot_removals.served(),
        history,
        recent: crate::blocks::Recent::new(),
        slowed: crate::health::Slowed::new(),
        started: crate::health::Started {
            at_ms: millis(&context),
            height: tip.height,
        },
        activity,
        logs,
        shutdown,
        tips,
        finalized: known.clone(),
        votes: steered
            .as_ref()
            .map_or_else(consensus::Votes::default, |(_, membership, _)| {
                membership.votes().clone()
            }),
        relay,
        blobs,
        halo,
        live,
        egress,
        crosspond,
        answered,
        queried,
        key: identity.clone(),
        mesh: transport::loopback(p2p),
        halo_self,
        forward,
        solo: solo_handle,
    });
    let unrecorded = steered
        .as_ref()
        .map(|(marshal, _, _)| (marshal.anchor(), marshal.mailbox().clone()));
    if let Some((first, archived)) = unrecorded {
        let founding = tip.height == 0;
        if let Anchor::Genesis(genesis) = first
            && founding
        {
            daemon
                .record_unrecorded(&node, genesis, genesis.digest(), None)
                .await?;
        }
        let applied_unsummarized = carried
            .unsummarized
            .into_iter()
            .filter(|height| *height <= tip.height);
        for height in applied_unsummarized {
            let asked = commonware_consensus::types::Height::new(height);
            let block = archived.get_block(asked).await.ok_or_else(|| {
                Error::Corrupt(format!(
                    "block {height} is archived and marshal does not read it"
                ))
            })?;
            let finalization = archived.get_finalization(asked).await;
            daemon
                .record_unrecorded(&node, &block, block.digest(), finalization)
                .await?;
        }
    }
    let (marshal, consensus) = match steered {
        Some((marshal, membership, _)) => {
            let consensus = Consensus {
                anchors: marshal.mailbox().clone(),
                membership,
                snapshots: crate::snapshot::Snapshots::new(
                    workspace.dir().join(crate::snapshot::DIR),
                    workspace.runtime_dir(),
                    crate::snapshot::reflink,
                    snapshot_removals,
                    known,
                    daemon.network.epoch_length,
                ),
            };
            (Some(marshal), Some(consensus))
        }
        None => (None, None),
    };
    let anchor = match &consensus {
        Some(consensus) => consensus.anchors.anchor(tip).await,
        None => None,
    };
    published(&node, &daemon, anchor);
    if let Some(head) = daemon.head(tip.height).await? {
        daemon.changes.opened(head);
    }

    let source = workspace.source()?.map(Client::new);
    let pump = context.child("pump").spawn({
        let writing = Writing {
            daemon: daemon.clone(),
            node,
            commands,
            consensus,
            tracker,
            tally,
            source: source.clone(),
            query_fuel: load.query_fuel.map(NonZeroU64::get),
            unmarked: None,
            workspace: workspace.clone(),
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
        queries,
        load.rpc_cpus,
        load.rpc_blocking_threads,
    )?;
    if let (Some(source), Some(handle)) = (source, rpc.handle()) {
        handle.spawn(crate::fill::fill(daemon.clone(), source));
    }
    if let Some(handle) = rpc.handle() {
        crate::forward::spawn(daemon.clone(), handle);
    }
    if let Some(handle) = rpc.handle() {
        crate::egress::lane::spawn(daemon.clone(), handle, load.egress_lane_backlog)?;
    }
    let anchors = marshal.as_ref().map(|marshal| marshal.mailbox().clone());
    let egressing = crate::egress::spawn(daemon.clone(), anchors);
    let reading = crate::reads::spawn(daemon.clone());
    let dialing = context.child("halo_self").spawn({
        let daemon = daemon.clone();
        move |_| crate::halo_self::dialing(daemon)
    });
    let mut tasks = vec![pump, relayed, egressing, reading, dialing];
    if let Some(driving) = driving {
        let driven = context
            .child("solo")
            .spawn(move |context| crate::fork::drive(context, driving));
        tasks.push(driven);
    }
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

fn loopback_only(listeners: &Listeners) -> Result<()> {
    let mut bound = vec![
        ("p2p", listeners.p2p.local_addr()?),
        ("http", listeners.http.local_addr()?),
        ("halo", listeners.halo.local_addr()?),
    ];
    if let Some(tls) = &listeners.tls {
        bound.push(("tls", tls.local_addr()?));
    }
    if let Some(live) = &listeners.live {
        bound.push(("live", live.local_addr()?));
    }
    let exposed: Vec<String> = bound
        .iter()
        .filter(|(_, address)| !address.ip().is_loopback())
        .map(|(socket, address)| format!("{socket} {address}"))
        .collect();
    if exposed.is_empty() {
        return Ok(());
    }
    Err(Error::Address(format!(
        "a fork takes ops as any signer, so it listens on loopback only, and {} is not",
        exposed.join(", ")
    )))
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
                https: relay_address(member.address.as_deref()),
                validator,
            })
        })
        .collect()
}

fn relay_address(address: Option<&str>) -> Option<String> {
    let endpoint: Endpoint = address?.parse().ok()?;
    let web = matches!(endpoint.scheme, Scheme::Ws | Scheme::Wss);
    web.then(|| endpoint.to_string())
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
    consensus: Option<Consensus<E>>,
    tracker: Tracker,
    tally: Tally,
    source: Option<Client>,
    query_fuel: Option<u64>,
    unmarked: Option<u64>,
    workspace: Workspace,
}

struct Consensus<E: Context> {
    anchors: MarshalMailbox,
    membership: Seating<E>,
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

fn slow_of(
    phases: &Phases,
    height: u64,
    block_time_ms: u64,
    commands: u64,
    commanding: Duration,
) -> Option<crate::health::Slow> {
    if !phases.slow(block_time_ms) {
        return None;
    }
    let slowest = phases
        .slowest
        .as_ref()
        .map(|slowest| crate::health::Slowest {
            ms: ms(slowest.spent),
            program: slowest.program.clone(),
            position: slowest.position,
            tx: slowest.tx.clone(),
        });
    Some(crate::health::Slow {
        height,
        total_ms: ms(phases.total),
        behind_ms: ms(phases.behind),
        commands,
        commands_ms: ms(commanding),
        executing_ms: ms(phases.executing),
        committing_ms: ms(phases.committing),
        verifying_ms: ms(phases.verifying()),
        publishing_ms: ms(phases.publishing),
        recording_ms: ms(phases.recording),
        proposer_ms: ms(phases.proposing),
        history_ms: ms(phases.keeping),
        receipting_ms: ms(phases.receipting),
        seating_ms: ms(phases.seating),
        snapshot_ms: ms(phases.snapshotting),
        slowest,
    })
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
            let slow = slow_of(
                &phases,
                block.height,
                block_time_ms,
                writing.tally.commands,
                writing.tally.commanding,
            );
            if let Some(slow) = slow {
                daemon.slowed.keep(slow);
            }
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
        consensus,
        tracker,
        unmarked,
        ..
    } = writing;
    daemon.hang_up().await;
    daemon.current.withdraw();
    daemon.pool.close().await;
    drop(commands);
    if let Some(consensus) = consensus {
        consensus.membership.stop().await;
    }
    drop(tracker);
    let closed = match unmarked {
        Some(height) => Ok(height),
        None => {
            let synced = node.close().await;
            synced
                .map_err(Error::from)
                .and_then(|()| Ok(node.tip()?.height))
        }
    };
    daemon.readers.closed().await;
    drop(node);
    let _ = closer.send(closed);
}

fn published<E: Context>(node: &Node<E>, daemon: &Daemon<E>, anchor: Option<Anchor>) {
    match View::of(node, &daemon.facts(), anchor) {
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
        let source = writing.source.clone();
        let fetching = async {
            match &source {
                Some(source) => {
                    daemon
                        .blobs
                        .fetch_or(id, halo::Class::Critical, source)
                        .await
                }
                None => daemon.blobs.fetch(id, halo::Class::Critical).await,
            }
        };
        futures::pin_mut!(fetching);
        let fetched = loop {
            let command = tokio::select! {
                fetched = &mut fetching => break fetched,
                command = writing.commands.next() => command,
            };
            match asked(&mut writing.tally, &daemon.context, command).await {
                Flow::Writing => {}
                Flow::Closing(closer) => return Ok(Held::Closing(closer)),
                Flow::Unasked => return Ok(Held::Unasked),
            }
        };
        let Fetched::Unheld = fetched else {
            continue;
        };
        let Some(source) = writing.source.clone() else {
            continue;
        };
        let past = writing.node.tip()?.height;
        let pause = Duration::from_millis(daemon.descriptor.block_time_ms);
        let waiting = Box::pin(statesync::ahead(
            daemon.context.child("readopt"),
            pause,
            source.clone(),
            past,
        ));
        match racing(writing, waiting).await {
            Raced::Done(ahead) => ahead?,
            Raced::Held(held) => return Ok(held),
        }
        let (parts, adoption) = writing.node.release().await?;
        writing.unmarked = Some(past);
        let adopting = Box::pin(statesync::readopt(
            daemon.context.child("readopt"),
            &daemon.descriptor.network,
            parts,
            adoption,
            daemon.descriptor.id(),
            pause,
            source.clone(),
            past,
        ));
        let joined = match racing(writing, adopting).await {
            Raced::Done(joined) => joined?,
            Raced::Held(held) => return Ok(held),
        };
        writing.node = joined.node;
        writing.unmarked = None;
        let tip = writing.node.tip()?;
        let head = match racing(writing, Box::pin(head_at(&source, tip))).await {
            Raced::Done(head) => head,
            Raced::Held(held) => return Ok(held),
        };
        Box::pin(readopted(writing, joined.anchor, head, past, block, id)).await?;
    }
}

async fn head_at(source: &Client, tip: host::Tip) -> Option<crate::wire::Head> {
    let ask = crate::wire::BlockAsk {
        by: crate::wire::BlockRef::Height(tip.height),
        most: 0,
    };
    let block = source.block(ask).await.ok().flatten()?;
    let named = block.id == tip.id;
    named.then_some(crate::wire::Head {
        height: block.height,
        id: block.id,
        time: block.time,
    })
}

enum Raced<T> {
    Done(T),
    Held(Held),
}

async fn racing<E: Context, T>(
    writing: &mut Writing<E>,
    work: impl Future<Output = T>,
) -> Raced<T> {
    let clock = writing.daemon.clone();
    futures::pin_mut!(work);
    loop {
        let command = tokio::select! {
            done = &mut work => return Raced::Done(done),
            command = writing.commands.next() => command,
        };
        match asked(&mut writing.tally, &clock.context, command).await {
            Flow::Writing => {}
            Flow::Closing(closer) => return Raced::Held(Held::Closing(closer)),
            Flow::Unasked => return Raced::Held(Held::Unasked),
        }
    }
}

async fn readopted<E: Context>(
    writing: &mut Writing<E>,
    anchor: Anchor,
    head: Option<crate::wire::Head>,
    past: u64,
    block: &Block,
    blob: abi::BlobId,
) -> Result<()> {
    let daemon = writing.daemon.clone();
    let Writing {
        node,
        consensus,
        tracker,
        query_fuel,
        ..
    } = writing;
    node.meter_queries(*query_fuel);
    let tip = node.tip()?;
    tracing::warn!(
        target: "ducktape::node",
        event = "state_readopted",
        height = tip.height,
        block = block.height,
        ?blob,
        reason = "blob_unheld",
        "the block reads a blob that no member gave and the source no longer serves; the node re-adopted the state at the source's head and goes on from there"
    );
    writing.workspace.write_anchor(&anchor)?;
    published(node, &daemon, Some(anchor.clone()));
    daemon.tips.send_replace(tip.height);
    let programs: Vec<abi::ProgramId> = node.host().store().programs().cloned().collect();
    daemon.readopted(programs, head).await;
    let blobs_role = node.host().role(role::BLOBS)?;
    let bundles: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> = node
        .view(Layer::Confirmed)
        .scan(blobs_role, &Scan::prefix(BUNDLES))
        .map_err(host::Error::from)?
        .into_iter()
        .map(|entry| (entry.key, Some(entry.value)))
        .collect();
    daemon.blobs.applied(tip.height, &bundles);
    retable_halo(&daemon, node, tip.height).await;
    let Some(consensus) = consensus.as_mut() else {
        daemon.pool.offer(Offer::Every);
        return Ok(());
    };
    for (epoch, keys) in recorded_epochs(node)? {
        let keys = validators_of(&keys).ok_or(Error::Corrupt(format!(
            "epoch {epoch} seats an undecodable key"
        )))?;
        consensus.membership.roster().seat(epoch, keys);
    }
    reseat_halo(&daemon, node, tip.height).await;
    let epoch = daemon.network.epoch_after(tip.height);
    let crossed = epoch != daemon.network.epoch_after(past);
    if crossed {
        let unrecorded =
            || Error::Corrupt(format!("the re-adopted state records no epoch {epoch}"));
        let members = node.epoch_members(epoch)?.ok_or_else(unrecorded)?;
        let validators = node.epoch_validators(epoch)?.ok_or_else(unrecorded)?;
        let seated = validators_of(&validators).ok_or(Error::Corrupt(format!(
            "epoch {epoch} seats an undecodable key"
        )))?;
        daemon.relay.seat(seated.as_ref());
        tracker.track(epoch, &members);
        consensus.membership.anchored(anchor);
        consensus
            .membership
            .seat(tip, node.host().root()?, &validators)
            .await?;
    }
    daemon.pool.offer(Offer::Every);
    Ok(())
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
        consensus,
        tracker,
        ..
    } = writing;
    let daemon: &Daemon<E> = daemon;
    let consenting = consensus.is_some();
    let anchor = match consensus.as_mut() {
        Some(consensus) => consensus.anchors.anchor(node.tip()?).await,
        None => None,
    };
    let finalization = match &anchor {
        Some(Anchor::Finalized(certificate)) => Some(certificate.clone()),
        Some(Anchor::Genesis(_)) | None => None,
    };
    published(node, daemon, anchor);
    let closes = daemon.network.closes_an_epoch(block.height);
    let replayed = matches!(sequenced, Sequenced::Replayed);
    let opens_an_epoch = !replayed && closes;
    let relays = !daemon.egress.current().config.crosspond.is_empty();
    let linked = relays || node.host().role(role::CROSSPOND).is_ok();
    let kept = closes && linked;
    if kept {
        record_close(daemon, node, block.height)?;
    }
    let opening = block
        .height
        .checked_sub(1)
        .is_some_and(|last| daemon.network.closes_an_epoch(last));
    if opening && linked {
        record_opening(daemon, node, block, finalization.as_ref(), relays)?;
    }
    let id = block.digest();
    daemon.votes.applied(id, block.height);
    daemon.tips.send_replace(block.height);
    phases.publishing = lap.lap();
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
            let recorded = daemon
                .record(node, block, id, ran, Some(receipts), finalization)
                .await?;
            phases.proposing = recorded.proposing;
            phases.keeping = recorded.keeping;
            daemon.activity.offer(crate::activity::Block {
                height: block.height,
                time: block.time,
                records,
                refused,
            });
            phases.recording = lap.lap();
            daemon.publish(
                &applied,
                crate::wire::Head {
                    height: block.height,
                    id: id.0,
                    time: block.time,
                },
            );
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
            if consenting {
                receipts_for(daemon, node, block.height, due, reseated)?;
            }
            phases.receipting = lap.lap();
            members_written
        }
        Sequenced::Replayed => {
            daemon
                .record_unrecorded(node, block, id, finalization)
                .await?;
            phases.recording = lap.lap();
            false
        }
    };
    let reseats = consenting && (members_written || closes);
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
    let Some(Consensus {
        membership,
        snapshots,
        ..
    }) = consensus.as_mut()
    else {
        phases.seating = lap.lap();
        return Ok(Flow::Writing);
    };
    // The mesh admits every member, so a resident follows the chain; only
    // the validators vote and propose.
    tracker.track(epoch, &members);
    membership
        .seat(block.tip(), node.host().root()?, &validators)
        .await?;
    phases.seating = lap.lap();
    let follows = !validators.contains(&daemon.identity);
    if follows {
        snapshots.take(node, block.height);
        phases.snapshotting = lap.lap();
    }
    Ok(Flow::Writing)
}

fn record_close<E: Context>(daemon: &Daemon<E>, node: &Node<E>, height: u64) -> Result<()> {
    let at_the_tip = node.tip()?.height == height;
    if !at_the_tip {
        return Ok(());
    }
    daemon.crosspond.close(node, &daemon.network, height)
}

fn record_opening<E: Context>(
    daemon: &Daemon<E>,
    node: &Node<E>,
    block: &Block,
    finalization: Option<&consensus::Certificate>,
    relays: bool,
) -> Result<()> {
    let at_the_tip = node.tip()?.height == block.height;
    let Some(certificate) = finalization.filter(|_| at_the_tip) else {
        return Ok(());
    };
    let epoch = daemon.network.epoch_after(block.height - 1);
    daemon.crosspond.open(crate::crosspond::Opened {
        epoch,
        block: block.encode().to_vec(),
        certificate: certificate.encode().to_vec(),
    })?;
    if relays {
        return Ok(());
    }
    daemon.crosspond.forget_before(epoch)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use futures::channel::mpsc;

    use abi::Outcome;

    use super::{
        Event, Phases, Slowest, listing, next, relay_address, rewrites, slow_of, slowest_of,
    };

    #[test]
    fn a_member_is_relayed_through_its_address_only_when_that_is_a_websocket() {
        let relayed = |address: Option<&str>| relay_address(address);
        assert_eq!(
            relayed(Some("wss://node.example")).as_deref(),
            Some("wss://node.example:443")
        );
        assert_eq!(
            relayed(Some("ws://localhost:8844")).as_deref(),
            Some("ws://localhost:8844")
        );
        assert_eq!(relayed(Some("tcp://node.example:9000")), None);
        assert_eq!(relayed(Some("tls://node.example:9443")), None);
        assert_eq!(relayed(Some("not an address")), None);
        assert_eq!(relayed(None), None);
    }

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
    fn a_slow_block_is_kept_with_its_phases_and_a_block_within_its_time_is_not() {
        let phases = Phases {
            total: Duration::from_millis(1500),
            behind: Duration::from_millis(40),
            applying: Duration::from_millis(1400),
            executing: Duration::from_millis(900),
            committing: Duration::from_millis(300),
            slowest: Some(Slowest {
                spent: Duration::from_millis(800),
                program: "harness".into(),
                position: 3,
                tx: "ab".into(),
            }),
            ..Phases::default()
        };
        let kept = slow_of(&phases, 7, 1000, 2, Duration::from_millis(5)).unwrap();
        assert_eq!(
            (
                kept.height,
                kept.total_ms,
                kept.behind_ms,
                kept.executing_ms,
                kept.committing_ms,
                kept.verifying_ms,
                kept.commands,
                kept.commands_ms,
            ),
            (7, 1500, 40, 900, 300, 200, 2, 5)
        );
        assert_eq!(
            kept.slowest,
            Some(crate::health::Slowest {
                ms: 800,
                program: "harness".into(),
                position: 3,
                tx: "ab".into(),
            })
        );
        assert_eq!(slow_of(&phases, 7, 1500, 0, Duration::ZERO), None);
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

    #[tokio::test]
    async fn seating_answers_its_changes_socket_while_it_asks_the_network() {
        use std::sync::Arc;

        use axum::extract::ws::{Message, WebSocketUpgrade};
        use axum::routing::get;
        use tokio::sync::Notify;

        use crate::wire::{Network, PeerStatus, route};

        let key = vec![7u8; 32];
        let ponged = Arc::new(Notify::new());
        let changes = {
            let ponged = ponged.clone();
            move |upgrade: WebSocketUpgrade| {
                let ponged = ponged.clone();
                async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        let ping = Message::Ping(1u64.to_le_bytes().to_vec().into());
                        if socket.send(ping).await.is_err() {
                            return;
                        }
                        while let Some(Ok(message)) = socket.recv().await {
                            if let Message::Pong(_) = message {
                                ponged.notify_one();
                            }
                        }
                    })
                }
            }
        };
        let network = {
            let ponged = ponged.clone();
            let key = key.clone();
            move || {
                let ponged = ponged.clone();
                let key = key.clone();
                async move {
                    ponged.notified().await;
                    let members = vec![PeerStatus { key, signed: None }];
                    abi::encode(&Network { height: 1, members })
                }
            }
        };
        let router = axum::Router::new()
            .route(&format!("{}/{{program}}", route::CHANGES), get(changes))
            .route(route::NETWORK, get(network));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let source = crate::Client::new(format!("http://{address}"));
        super::seated(&source, &key).await.unwrap();
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
