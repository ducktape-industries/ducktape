use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use abi::Outcome;
use abi::Scan;
use abi::role;
use abi::role::validators::{self, Grant, INVITE_NAMESPACE, Invite, Member, Op};
use blob::contract::{BUNDLES, Bundle};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_cryptography::{Digestible as _, Signer as _};
use commonware_p2p::authenticated::lookup::{Oracle, Receiver, Sender};
use commonware_runtime::Handle;
use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use consensus::{
    Ancestry, Anchor, Chain, EngineMux, Marshal, Membership, Network, Roster, Transport,
    validators_of,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use halo::{Halo, Seat};
use host::Layer;
use node::{Block, Frame, Node, Preconfirm, Proposer, Sequenced, Source, Taken};
use tokio::sync::watch;

use crate::endpoint::Endpoint;
use crate::mesh::{Mesh, Tracker};
use crate::relay::{self, Relay};
use crate::transport;
use crate::workspace::{Descriptor, Founding, Workspace};
use crate::{Client, Context, Daemon, Error, Logs, Result, Shared};

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Delivery = (Arc<Block>, Exact);
type Seating<E> =
    Membership<E, Sender<PublicKey, E>, Receiver<PublicKey>, Oracle<PublicKey>, Participant>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listen {
    pub p2p: SocketAddr,
    pub http: SocketAddr,
    pub tls: Option<SocketAddr>,
    pub halo: SocketAddr,
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

const READS_IN_FLIGHT: NonZeroUsize = NonZeroUsize::new(64).expect("nonzero");
const UPLOADS_IN_FLIGHT: NonZeroUsize = NonZeroUsize::new(8).expect("nonzero");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadConfig {
    pub reads_in_flight: NonZeroUsize,
    pub uploads_in_flight: NonZeroUsize,
    pub blob_backlog: NonZeroUsize,
    pub blob_serves_in_flight: NonZeroUsize,
    pub blob_gatherers: NonZeroUsize,
}

impl Default for LoadConfig {
    fn default() -> LoadConfig {
        let blobs = blob::service::Config::default();
        LoadConfig {
            reads_in_flight: READS_IN_FLIGHT,
            uploads_in_flight: UPLOADS_IN_FLIGHT,
            blob_backlog: blobs.backlog,
            blob_serves_in_flight: blobs.serving,
            blob_gatherers: blobs.gatherers,
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
}

pub struct Running<E: Context> {
    pub http: SocketAddr,
    pub p2p: SocketAddr,
    pub daemon: Arc<Daemon<E>>,
    _marshal: Marshal,
    _mesh: Mesh<E>,
    _tasks: Vec<Handle<()>>,
}

impl<E: Context> Running<E> {
    pub async fn stopped(self) {
        let mut down = self.daemon.shutdown.subscribe();
        let _ = down.wait_for(|down| *down).await;
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
) -> Boxed<'a, Result<Running<E>>> {
    Box::pin(start(context, workspace, logs, listen, halo, load))
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
    let halo = crate::workspace::halo_address(&halo)?;
    let status = source.status().await?;
    let identity = workspace.identity_or_create(&mut context)?;
    let salt = rand_core::Rng::next_u64(&mut context);
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
    seated(&source, identity.public_key().as_ref()).await?;
    let descriptor = Descriptor {
        network: status.network,
        time: status.time,
        block_time_ms: status.block_time_ms,
    };
    let joined = statesync::join(
        context.child("join"),
        &descriptor.network,
        workspace.dir(),
        descriptor.id(),
        source,
    )
    .await?;
    workspace.write_descriptor(&descriptor)?;
    workspace.write_anchor(&joined.anchor)
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
    listen: Listen,
    halo: HaloConfig,
    load: LoadConfig,
) -> Result<Running<E>> {
    let descriptor = workspace.descriptor()?;
    let identity = workspace.identity()?;
    let anchor = workspace.anchor()?;
    let egress = workspace.egress()?;
    let clients = crate::egress::Clients::new(egress.reach).map_err(Error::Egress)?;
    let node = Node::open(context.child("node"), &descriptor.network, workspace.dir()).await?;
    let network = Network {
        epoch_length: node.epoch_length()?,
        cadence: descriptor.cadence(),
    };
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
                udp: listen.halo,
                queues: halo.queues(),
            },
        )
        .map_err(|error| Error::Tls(error.to_string()))?,
    );
    halo.seat(halo_seats(&node, &network).await?);
    let blobs = blob::service::Service::start(
        halo.clone(),
        node.host().blobs().clone(),
        bundles(&node)?,
        load.blobs(),
    )?;

    let member_cap = node.member_cap()?;
    let (mesh, mut tracker, marshal_lanes, engine_channels, (relay_sender, relay_receiver)) =
        Mesh::start(
            context.child("mesh"),
            identity.clone(),
            &descriptor.id(),
            listen.p2p,
            member_cap,
        )?;
    tracker.track(epoch, &members);
    let roster = Roster::new(descriptor.id(), Some(identity.clone()));
    for (epoch, validators) in &seated {
        let validators = validators_of(validators).ok_or(Error::Corrupt(format!(
            "epoch {epoch} seats an undecodable key"
        )))?;
        roster.seat(*epoch, validators);
    }

    let (inbox, deliveries) = mpsc::unbounded();
    let chain = Participant {
        proposer: node.proposer(),
        inbox,
    };
    let node: Shared<E> = Arc::new(futures::lock::Mutex::new(node));
    let (marshal, summaries) = Marshal::start(
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
    .await;
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
        "the node is up"
    );

    let (shutdown, _) = watch::channel(false);
    let (tips, _) = watch::channel(tip.height);
    let daemon = Arc::new(Daemon {
        context: context.child("daemon"),
        node,
        descriptor: descriptor.clone(),
        network,
        identity: identity.public_key().as_ref().to_vec(),
        anchors: marshal.mailbox().clone(),
        summaries,
        logs,
        shutdown,
        tips,
        votes: membership.votes().clone(),
        relay: Relay::new(identity.public_key(), relay_sender, voters.as_ref()),
        blobs,
        halo,
        key: identity.clone(),
        mesh: transport::loopback(listen.p2p),
        subscribers: Mutex::new(Vec::new()),
    });
    let founding = tip.height == 0;
    if let Anchor::Genesis(genesis) = marshal.anchor()
        && founding
    {
        let node = daemon.node.lock().await;
        daemon.record_unrecorded(&node, genesis).await?;
    }

    let pump = context.child("pump").spawn({
        let daemon = daemon.clone();
        move |_| pump(daemon, membership, tracker, deliveries)
    });
    let relayed = context.child("relay").spawn({
        let daemon = daemon.clone();
        move |_| relay::serve(daemon, relay_receiver)
    });
    let listener = tokio::net::TcpListener::bind(listen.http).await?;
    let http = listener.local_addr()?;
    let tls = match listen.tls {
        Some(socket) => Some((
            tokio::net::TcpListener::bind(socket).await?,
            transport::acceptor()?,
        )),
        None => None,
    };
    let server = context.child("http").spawn({
        let router = crate::server::router(daemon.clone(), load);
        let mut down = daemon.shutdown.subscribe();
        move |_| async move {
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = down.wait_for(|down| *down).await;
                })
                .await;
            if let Err(error) = served {
                tracing::error!(target: "ducktape::http", %error, "the http server stopped");
            }
        }
    });
    let egressing = crate::egress::spawn(daemon.clone(), egress, clients);
    let mut tasks = vec![pump, server, relayed, egressing];
    if let Some((listener, acceptor)) = tls {
        let mesh = transport::loopback(listen.p2p);
        tasks.push(
            context
                .child("tls")
                .spawn(move |_| transport::inbound_tls(listener, acceptor, mesh)),
        );
    }
    workspace.write_http(http)?;
    Ok(Running {
        http,
        p2p: listen.p2p,
        daemon,
        _marshal: marshal,
        _mesh: mesh,
        _tasks: tasks,
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

#[derive(Clone)]
pub struct Participant {
    proposer: Proposer,
    inbox: mpsc::UnboundedSender<Delivery>,
}

impl Chain for Participant {
    async fn propose(
        &mut self,
        parent: Arc<Block>,
        ancestors: impl Ancestry<Block>,
        time: u64,
    ) -> Option<Block> {
        let applied = self.proposer.applied().height;
        let unapplied = consensus::unapplied(parent.clone(), ancestors, applied).await;
        let block = self
            .proposer
            .build(parent.tip(), unapplied.iter().map(AsRef::as_ref), time);
        Some(block)
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.inbox.unbounded_send((block, ack));
    }
}

async fn pump<E: Context>(
    daemon: Arc<Daemon<E>>,
    mut membership: Seating<E>,
    mut tracker: Tracker,
    mut deliveries: mpsc::UnboundedReceiver<Delivery>,
) {
    while let Some((block, ack)) = deliveries.next().await {
        let outcome = applied(&daemon, &mut membership, &mut tracker, &block).await;
        if let Err(error) = outcome {
            tracing::error!(
                target: "ducktape::node",
                event = "apply_failed",
                height = block.height,
                %error,
                "the node cannot apply a finalized block and stops"
            );
            daemon.shutdown.send_replace(true);
            return;
        }
        ack.acknowledge();
    }
}

async fn holding<'a, E: Context>(
    daemon: &'a Daemon<E>,
    block: &Block,
) -> Result<(futures::lock::MutexGuard<'a, Node<E>>, Sequenced)> {
    loop {
        let mut node = daemon.node.lock().await;
        let applied = node.apply(block).await;
        let Err(node::Error::Host(host::Error::BlobUnavailable(id))) = applied else {
            return Ok((node, applied?));
        };
        drop(node);
        tracing::warn!(
            target: "ducktape::node",
            event = "blob_unavailable",
            height = block.height,
            ?id,
            reason = "blob_missing",
            "the block reads a blob this node lacks; the node fetches it and applies again"
        );
        daemon.blobs.fetch(id, halo::Class::Critical).await;
    }
}

enum Posted {
    Pending,
    Retaken(Vec<u8>),
    Refused,
    Unheld,
}

async fn reposted<E: Context>(node: &mut Node<E>, posted: Option<Vec<u8>>) -> Result<Posted> {
    let Some(receipt) = posted else {
        return Ok(Posted::Unheld);
    };
    match node
        .submit(receipt.clone(), Source::Client, Preconfirm::Off)
        .await?
    {
        Ok(Taken::Held(_)) => Ok(Posted::Pending),
        Ok(Taken::New(_)) => Ok(Posted::Retaken(receipt)),
        Err(refusal) => {
            let carried = refusal.reason == abi::reason::DUPLICATE;
            if carried {
                return Ok(Posted::Refused);
            }
            Ok(Posted::Unheld)
        }
    }
}

async fn receipt_for<E: Context>(
    daemon: &Daemon<E>,
    node: &mut Node<E>,
    height: u64,
    bundle: u64,
) -> Result<Option<Vec<u8>>> {
    let tip = node.tip()?;
    let role = node.host().role(role::BLOBS)?.clone();
    let receipt = Frame::sign(
        &daemon.key,
        &daemon.descriptor.id(),
        tip,
        Some(height),
        &role,
        abi::encode(&blob::contract::Op::Receipt { bundle }),
    )
    .encode();
    match node
        .submit(receipt.clone(), Source::Client, Preconfirm::Off)
        .await?
    {
        Ok(Taken::New(_)) => {
            daemon.blobs.posted(bundle, receipt.clone());
            Ok(Some(receipt))
        }
        Ok(Taken::Held(_)) => Ok(None),
        Err(refusal) => {
            tracing::debug!(
                target: "ducktape::blob",
                bundle,
                reason = refusal.reason,
                "the node did not take its own receipt"
            );
            Ok(None)
        }
    }
}

async fn receipts_for<E: Context>(
    daemon: &Daemon<E>,
    node: &mut Node<E>,
    height: u64,
    due: Vec<blob::service::Due>,
    reseated: bool,
) -> Result<Vec<Vec<u8>>> {
    let mut taken = Vec::new();
    for due in due {
        match reposted(node, due.posted).await? {
            Posted::Pending => {}
            Posted::Retaken(receipt) => taken.push(receipt),
            Posted::Refused => {
                if reseated {
                    taken.extend(receipt_for(daemon, node, height, due.bundle).await?);
                }
            }
            Posted::Unheld => taken.extend(receipt_for(daemon, node, height, due.bundle).await?),
        }
    }
    Ok(taken)
}

async fn applied<E: Context>(
    daemon: &Daemon<E>,
    membership: &mut Seating<E>,
    tracker: &mut Tracker,
    block: &Block,
) -> Result<()> {
    let (mut node, sequenced) = holding(daemon, block).await?;
    // under the node lock, which a /v1/network read holds across the tip
    // and the book, so no height it serves passes its tip
    daemon.votes.applied(block.digest(), block.height);
    daemon.tips.send_replace(block.height);
    let closes = daemon.network.closes_an_epoch(block.height);
    let (receipted, members_written) = match sequenced {
        Sequenced::Applied {
            mut applied,
            carried,
        } => {
            let ran = crate::blocks::ran(block, &carried)?;
            let receipts = std::mem::take(&mut applied.submissions);
            daemon.record(&node, block, ran, Some(receipts)).await?;
            daemon.publish(&applied);
            let validators = node.host().role(role::VALIDATORS)?;
            let members_written = applied
                .writes
                .programs
                .get(validators)
                .is_some_and(|writes| !writes.is_empty());
            let blobs = node.host().role(role::BLOBS)?;
            let writes = applied.writes.programs.get(blobs).cloned();
            let due = daemon
                .blobs
                .applied(block.height, &writes.unwrap_or_default());
            let reseated = members_written || closes;
            let receipted = receipts_for(daemon, &mut node, block.height, due, reseated).await?;
            (receipted, members_written)
        }
        Sequenced::Replayed => {
            daemon.record_unrecorded(&node, block).await?;
            (Vec::new(), false)
        }
    };
    let reseats = members_written || closes;
    if reseats {
        reseat_halo(daemon, &node, block.height).await;
    }
    if !closes {
        let due = node.reoffer();
        drop(node);
        daemon
            .relay
            .send(due.iter().chain(&receipted).map(Vec::as_slice));
        return Ok(());
    }
    let epoch = daemon.network.epoch_after(block.height);
    let unrecorded = || Error::Corrupt(format!("the boundary block records no epoch {epoch}"));
    let members = node.epoch_members(epoch)?.ok_or_else(unrecorded)?;
    let validators = node.epoch_validators(epoch)?.ok_or_else(unrecorded)?;
    let seated = validators_of(&validators).ok_or(Error::Corrupt(format!(
        "epoch {epoch} seats an undecodable key"
    )))?;
    let reseated = daemon.relay.seat(seated.as_ref());
    let due = if reseated {
        node.reoffer_all()
    } else {
        node.reoffer()
    };
    drop(node);
    // The mesh admits every member, so a resident follows the chain; only
    // the validators vote and propose.
    tracker.track(epoch, &members);
    daemon
        .relay
        .send(due.iter().chain(&receipted).map(Vec::as_slice));
    membership.seat(block.tip(), &validators).await?;
    Ok(())
}
