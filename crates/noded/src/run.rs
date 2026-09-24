use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use abi::Outcome;
use abi::admission::{self, Grant, Invite};
use abi::valset::Seating;
use commonware_cryptography::Signer as _;
use commonware_cryptography::ed25519::{PrivateKey, PublicKey};
use commonware_p2p::authenticated::lookup::{Oracle, Receiver, Sender};
use commonware_runtime::Handle;
use commonware_utils::Acknowledgement as _;
use commonware_utils::acknowledgement::Exact;
use consensus::{
    Anchor, Chain, EngineMux, Marshal, Membership, Network, Roster, Transport, validators_of,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use node::{Block, Node, Sequenced};
use tokio::sync::watch;

use crate::mesh::{Mesh, Reach, track};
use crate::relay::{self, Relay};
use crate::workspace::{Descriptor, Founding, Workspace};
use crate::{Client, Context, Daemon, Error, Logs, Result, Shared};

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type Delivery = (Arc<Block>, Exact);
type Seats<E> =
    Membership<E, Sender<PublicKey, E>, Receiver<PublicKey>, Oracle<PublicKey>, Participant<E>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listen {
    pub p2p: SocketAddr,
    pub http: SocketAddr,
    pub reach: Reach,
}

pub struct Running<E: Context> {
    pub http: SocketAddr,
    pub p2p: SocketAddr,
    pub daemon: Arc<Daemon<E>>,
    _marshal: Marshal,
    _mesh: Mesh<E>,
    _tasks: [Handle<()>; 3],
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
    address: SocketAddr,
    invite: Option<Invite>,
) -> Boxed<'a, Result<()>> {
    Box::pin(adopt(context, workspace, source, address, invite))
}

pub fn run<'a, E: Context>(
    context: E,
    workspace: &'a Workspace,
    logs: Logs,
    listen: Listen,
) -> Boxed<'a, Result<Running<E>>> {
    Box::pin(start(context, workspace, logs, listen))
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
    address: SocketAddr,
    invite: Option<Invite>,
) -> Result<()> {
    let status = source.status().await?;
    let descriptor = Descriptor {
        network: status.network,
        time: status.time,
        block_time_ms: status.block_time_ms,
    };
    let identity = workspace.identity_or_create(&mut context)?;
    let joined = statesync::join(
        context.child("join"),
        &descriptor.network,
        workspace.dir(),
        descriptor.id(),
        source.clone(),
    )
    .await?;
    workspace.write_descriptor(&descriptor)?;
    workspace.write_anchor(&joined.anchor)?;
    enroll(&source, &identity, address, invite).await
}

async fn enroll(
    source: &Client,
    identity: &PrivateKey,
    address: SocketAddr,
    invite: Option<Invite>,
) -> Result<()> {
    let enroll = admission::Op::Enroll {
        address: address.to_string(),
        invite,
    };
    let receipt = source
        .submit_signed(identity, admission::PROGRAM, abi::encode(&enroll))
        .await?;
    match receipt.outcome {
        Outcome::Applied { .. } => Ok(()),
        Outcome::Rejected(refusal) => Err(Error::Refused(refusal)),
    }
}

pub fn invite(identity: &PrivateKey, grant: Grant) -> Invite {
    let signature = identity
        .sign(admission::INVITE_NAMESPACE, &grant.preimage())
        .as_ref()
        .to_vec();
    Invite {
        issuer: identity.public_key().as_ref().to_vec(),
        grant,
        signature,
    }
}

async fn start<E: Context>(
    context: E,
    workspace: &Workspace,
    logs: Logs,
    listen: Listen,
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
    let seating = seated.get(&epoch).cloned().ok_or(Error::Corrupt(format!(
        "the state seats nobody for epoch {epoch}"
    )))?;

    let (mesh, marshal_lanes, engine_channels, (relay_sender, relay_receiver)) = Mesh::start(
        context.child("mesh"),
        identity.clone(),
        &descriptor.id(),
        listen.p2p,
        listen.reach,
    );
    let mut oracle = mesh.oracle();
    track(&mut oracle, epoch, &seating, identity.public_key().as_ref());
    let roster = Roster::new(descriptor.id(), Some(identity.clone()));
    for (epoch, seating) in &seated {
        let validators = validators_of(&seating.validators).ok_or(Error::Corrupt(format!(
            "epoch {epoch} seats an undecodable key"
        )))?;
        roster.seat(*epoch, validators);
    }

    let node: Shared<E> = Arc::new(futures::lock::Mutex::new(node));
    let (inbox, deliveries) = mpsc::unbounded();
    let chain = Participant {
        node: node.clone(),
        inbox,
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
    let standing = membership.seat(tip, &seating).await?;
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
        logs,
        shutdown,
        relay: Relay::new(identity.public_key(), relay_sender, &seating),
        subscribers: Mutex::new(Vec::new()),
    });

    let pump = context.child("pump").spawn({
        let daemon = daemon.clone();
        move |_| pump(daemon, membership, oracle, deliveries)
    });
    let relayed = context.child("relay").spawn({
        let daemon = daemon.clone();
        move |_| relay::serve(daemon, relay_receiver)
    });
    let listener = tokio::net::TcpListener::bind(listen.http).await?;
    let http = listener.local_addr()?;
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
    workspace.write_http(http)?;
    Ok(Running {
        http,
        p2p: listen.p2p,
        daemon,
        _marshal: marshal,
        _mesh: mesh,
        _tasks: [pump, server, relayed],
    })
}

fn recorded_epochs<E: Context>(node: &Node<E>) -> Result<std::collections::BTreeMap<u64, Seating>> {
    let mut seated = std::collections::BTreeMap::new();
    let mut epoch = 0;
    while let Some(seating) = node.epoch_seating(epoch)? {
        seated.insert(epoch, seating);
        epoch += 1;
    }
    Ok(seated)
}

pub struct Participant<E: Context> {
    node: Shared<E>,
    inbox: mpsc::UnboundedSender<Delivery>,
}

impl<E: Context> Clone for Participant<E> {
    fn clone(&self) -> Self {
        Participant {
            node: self.node.clone(),
            inbox: self.inbox.clone(),
        }
    }
}

impl<E: Context> Chain for Participant<E> {
    async fn propose(&mut self, parent: Arc<Block>, time: u64) -> Option<Block> {
        let node = self.node.lock().await;
        Some(node.build(parent.tip(), time))
    }

    fn deliver(&mut self, block: Arc<Block>, ack: Exact) {
        let _ = self.inbox.unbounded_send((block, ack));
    }
}

async fn pump<E: Context>(
    daemon: Arc<Daemon<E>>,
    mut membership: Seats<E>,
    mut oracle: Oracle<PublicKey>,
    mut deliveries: mpsc::UnboundedReceiver<Delivery>,
) {
    while let Some((block, ack)) = deliveries.next().await {
        let outcome = applied(&daemon, &mut membership, &mut oracle, &block).await;
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

async fn applied<E: Context>(
    daemon: &Daemon<E>,
    membership: &mut Seats<E>,
    oracle: &mut Oracle<PublicKey>,
    block: &Block,
) -> Result<()> {
    let mut node = daemon.node.lock().await;
    if let Sequenced::Applied(outcome) = node.apply(block).await? {
        daemon.publish(&outcome);
    }
    if !daemon.network.closes_an_epoch(block.height) {
        return Ok(());
    }
    let epoch = daemon.network.epoch_after(block.height);
    let seating = node.epoch_seating(epoch)?.ok_or(Error::Corrupt(format!(
        "the boundary block records no epoch {epoch}"
    )))?;
    let waiting: Vec<Vec<u8>> = node.waiting().map(<[u8]>::to_vec).collect();
    drop(node);
    track(oracle, epoch, &seating, &daemon.identity);
    daemon.relay.seat(&seating);
    daemon.relay.send(waiting.iter().map(Vec::as_slice));
    membership.seat(block.tip(), &seating).await?;
    Ok(())
}
