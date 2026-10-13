use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use abi::Outcome;
use abi::role::validators::{Grant, INVITE_NAMESPACE, Invite, Op};
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
use halo::{Endpoint, Halo};
use node::{Block, Frame, Node, Sequenced};
use tokio::sync::watch;

use crate::fetch;
use crate::mesh::{Mesh, Tracker};
use crate::relay::{self, Relay};
use crate::transport;
use crate::workspace::{Descriptor, Founding, Workspace};
use crate::{Client, Context, Daemon, Error, Logs, Result, Shared};

const HALO_NAMESPACE: &[u8] = b"/halo";

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Delivery = (Arc<Block>, Exact);
type Seating<E> =
    Membership<E, Sender<PublicKey, E>, Receiver<PublicKey>, Oracle<PublicKey>, Participant<E>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listen {
    pub p2p: SocketAddr,
    pub http: SocketAddr,
    pub tls: Option<SocketAddr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HaloConfig {
    pub egress_bytes_per_sec: u64,
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
) -> Boxed<'a, Result<()>> {
    Box::pin(found(context, workspace, founding))
}

pub fn join<'a, E: Context>(
    context: E,
    workspace: &'a Workspace,
    source: Client,
    address: Option<Endpoint>,
    invite: Option<Invite>,
) -> Boxed<'a, Result<()>> {
    Box::pin(adopt(context, workspace, source, address, invite))
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
) -> Boxed<'a, Result<Running<E>>> {
    Box::pin(start(context, workspace, logs, listen, halo))
}

async fn found<E: Context>(mut context: E, workspace: &Workspace, founding: &Path) -> Result<()> {
    let (founding, base) = Founding::read(founding)?;
    workspace.identity_or_create(&mut context)?;
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
    invite: Option<Invite>,
) -> Result<()> {
    let status = source.status().await?;
    let identity = workspace.identity_or_create(&mut context)?;
    let salt = rand_core::Rng::next_u64(&mut context);
    let enroll = Op::Enroll {
        address: address.as_ref().map(Endpoint::to_string),
        invite,
    };
    let frame = Frame::sign(
        &identity,
        status.network.as_bytes(),
        status.anchor(),
        Some(salt),
        &status.roles.validators,
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
) -> Result<Running<E>> {
    let descriptor = workspace.descriptor()?;
    let identity = workspace.identity()?;
    let anchor = workspace.anchor()?;
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

    let member_cap = node.member_cap()?;
    let p2p = tokio::net::TcpListener::bind(listen.p2p).await?;
    let tls = match listen.tls {
        Some(socket) => Some((
            tokio::net::TcpListener::bind(socket).await?,
            halo::dial::acceptor().map_err(|error| Error::Tls(error.to_string()))?,
        )),
        None => None,
    };
    let internal = transport::internal()?;
    let (mesh, mut tracker, marshal_lanes, engine_channels, (relay_sender, relay_receiver)) =
        Mesh::start(
            context.child("mesh"),
            identity.clone(),
            &descriptor.id(),
            internal,
            member_cap,
        )?;
    tracker.track(epoch, &members);
    let udp = std::iter::once(&p2p)
        .chain(tls.as_ref().map(|(listener, _)| listener))
        .map(tokio::net::TcpListener::local_addr)
        .collect::<std::io::Result<Vec<_>>>()?;
    let halo = Arc::new(
        Halo::start(
            context.child("halo"),
            identity.clone(),
            &[descriptor.id().as_slice(), HALO_NAMESPACE].concat(),
            halo::Config {
                egress_bytes_per_sec: halo.egress_bytes_per_sec,
                udp,
            },
            node.host().blobs().clone(),
        )
        .map_err(|error| Error::Tls(error.to_string()))?,
    );
    halo.seat(fetch::seats(&members, &validators));
    let roster = Roster::new(descriptor.id(), Some(identity.clone()));
    for (epoch, validators) in &seated {
        let validators = validators_of(validators).ok_or(Error::Corrupt(format!(
            "epoch {epoch} seats an undecodable key"
        )))?;
        roster.seat(*epoch, validators);
    }

    let node: Shared<E> = Arc::new(futures::lock::Mutex::new(node));
    let (inbox, deliveries) = mpsc::unbounded();
    let chain = Participant {
        node: node.clone(),
        halo: halo.clone(),
        inbox,
    };
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
        votes: membership.votes().clone(),
        relay: Relay::new(identity.public_key(), relay_sender, voters.as_ref()),
        halo: halo.clone(),
        mesh: internal,
        subscribers: Mutex::new(Vec::new()),
        recent: Mutex::default(),
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
    let p2p_at = p2p.local_addr()?;
    let server = context.child("http").spawn({
        let router = crate::server::router(daemon.clone());
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
    let inbound = context.child("p2p").spawn({
        let halo = halo.clone();
        move |_| transport::inbound_p2p(p2p, halo, internal)
    });
    let mut tasks = vec![pump, server, relayed, inbound];
    if let Some((listener, acceptor)) = tls {
        tasks.push(
            context
                .child("tls")
                .spawn(move |_| transport::inbound_tls(listener, acceptor, halo, internal)),
        );
    }
    workspace.write_http(http)?;
    Ok(Running {
        http,
        p2p: p2p_at,
        daemon,
        _marshal: marshal,
        _mesh: mesh,
        _tasks: tasks,
    })
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
    node: Shared<E>,
    halo: Arc<Halo<E>>,
    inbox: mpsc::UnboundedSender<Delivery>,
}

impl<E: Context> Clone for Participant<E> {
    fn clone(&self) -> Self {
        Participant {
            node: self.node.clone(),
            halo: self.halo.clone(),
            inbox: self.inbox.clone(),
        }
    }
}

impl<E: Context> Participant<E> {
    async fn build(
        &self,
        parent: Arc<Block>,
        ancestors: impl Ancestry<Block>,
        time: u64,
    ) -> node::Result<Block> {
        let applied = self.node.lock().await.tip()?.height;
        let unapplied = consensus::unapplied(parent.clone(), ancestors, applied).await;
        let node = self.node.lock().await;
        node.build(parent.tip(), unapplied.iter().map(AsRef::as_ref), time)
    }
}

impl<E: Context> Chain for Participant<E> {
    async fn propose(
        &mut self,
        parent: Arc<Block>,
        ancestors: impl Ancestry<Block>,
        time: u64,
    ) -> Option<Block> {
        let height = parent.height + 1;
        match self.build(parent, ancestors, time).await {
            Ok(block) => Some(block),
            Err(error) => {
                tracing::error!(
                    target: "ducktape::node",
                    event = "build_failed",
                    height,
                    %error,
                    "the node cannot build a block"
                );
                None
            }
        }
    }

    async fn verify(&mut self, leader: PublicKey, block: Arc<Block>) -> bool {
        let held = fetch::hold_block(&self.node, &*self.halo, &block, Some(leader)).await;
        let Err(error) = held else {
            return true;
        };
        tracing::error!(
            target: "ducktape::node",
            event = "verify_failed",
            height = block.height,
            %error,
            "the node cannot hold what a proposed block attaches and abstains"
        );
        futures::future::pending().await
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

async fn apply_holding<'a, E: Context>(
    daemon: &'a Daemon<E>,
    block: &Block,
) -> Result<(futures::lock::MutexGuard<'a, Node<E>>, Sequenced)> {
    loop {
        let mut node = daemon.node.lock().await;
        match node.apply(block).await {
            Err(node::Error::Host(host::Error::BlobUnavailable(id))) => {
                drop(node);
                tracing::warn!(
                    target: "ducktape::node",
                    event = "blob_unavailable",
                    height = block.height,
                    ?id,
                    reason = "blob_missing",
                    "the block reads a blob this node lacks; the node fetches it and applies again"
                );
                fetch::repair(&daemon.node, &*daemon.halo, id).await?;
            }
            sequenced => return Ok((node, sequenced?)),
        }
    }
}

async fn applied<E: Context>(
    daemon: &Daemon<E>,
    membership: &mut Seating<E>,
    tracker: &mut Tracker,
    block: &Block,
) -> Result<()> {
    fetch::hold_block(&daemon.node, &*daemon.halo, block, None).await?;
    let (mut node, sequenced) = apply_holding(daemon, block).await?;
    // under the node lock, which a /v1/network read holds across the tip
    // and the book, so no height it serves passes its tip
    daemon.votes.applied(block.digest(), block.height);
    match sequenced {
        Sequenced::Applied {
            mut applied,
            carried,
        } => {
            let ran = crate::blocks::ran(block, &carried)?;
            let receipts = std::mem::take(&mut applied.submissions);
            daemon.record(&node, block, ran, Some(receipts)).await?;
            daemon.publish(&applied);
        }
        Sequenced::Replayed => daemon.record_unrecorded(&node, block).await?,
    }
    if !daemon.network.closes_an_epoch(block.height) {
        let due = node.reoffer()?;
        drop(node);
        daemon.relay.send(due.iter().map(Vec::as_slice));
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
        node.reoffer_all()?
    } else {
        node.reoffer()?
    };
    drop(node);
    // The mesh admits every member, so a resident follows the chain; only
    // the validators vote and propose.
    tracker.track(epoch, &members);
    daemon.halo.seat(fetch::seats(&members, &validators));
    daemon.relay.send(due.iter().map(Vec::as_slice));
    membership.seat(block.tip(), &validators).await?;
    Ok(())
}
