mod blocks;
mod client;
mod config;
mod egress;
mod endpoint;
mod link;
pub mod live;
mod logs;
mod mesh;
mod network;
mod relay;
mod run;
mod server;
mod transport;
pub mod wire;
mod workspace;

use std::sync::{Arc, Mutex};

use abi::{ProgramId, Refusal};
use commonware_cryptography::Digestible as _;
use consensus::{MarshalMailbox, Network, Summaries, Votes};
use futures::channel::mpsc;
use host::Applied;
use node::Node;
use tokio::sync::watch;

pub use client::{Client, Upload};
pub use config::{
    ConsensusSettings, DEFAULT_HTTP, DEFAULT_LOG, LoadSettings, NodeConfig, Settings, permits,
};
pub use endpoint::{Endpoint, Scheme};
pub use logs::Logs;
pub use mesh::Mesh;
pub use run::{
    HaloConfig, Listen, Listeners, LoadConfig, Running, init, invite, join, run, run_on,
};
pub use wire::{NODE_CONTRACT, Status};
pub use workspace::{Descriptor, Founding, Workspace};

use crate::wire::Change;

pub trait Context:
    consensus::Context + commonware_runtime::Network + commonware_runtime::Resolver
{
}

impl<E> Context for E where
    E: consensus::Context + commonware_runtime::Network + commonware_runtime::Resolver
{
}

pub type Shared<E> = Arc<futures::lock::Mutex<Node<E>>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("the founding file does not parse: {0}")]
    Founding(#[from] toml::de::Error),
    #[error("{} is refused: {reason}", .path.display())]
    Settings {
        path: std::path::PathBuf,
        reason: String,
    },
    #[error("the node has no p2p socket: pass --listen or set listen in node.toml")]
    NoListen,
    #[error("the node has no halo socket: pass --halo or set halo in node.toml")]
    NoHalo,
    #[error("the consensus timeouts are refused: {0}")]
    Cadence(#[from] consensus::CadenceError),
    #[error("{count} is past the {most} a tokio semaphore holds")]
    PastPermits {
        count: std::num::NonZeroUsize,
        most: usize,
    },
    #[error("not hex: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("{0}")]
    Address(String),
    #[error("tls: {0}")]
    Tls(String),
    #[error("the workspace is corrupt: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Node(#[from] node::Error),
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error(transparent)]
    Blob(#[from] blob::Error),
    #[error(transparent)]
    Sync(#[from] statesync::Error),
    #[error(transparent)]
    Summaries(#[from] consensus::SummariesError),
    #[error(transparent)]
    Membership(#[from] consensus::MembershipError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Ws(Box<tokio_tungstenite::tungstenite::Error>),
    #[error("the node's answer does not decode: {}", .0.sentence)]
    Decode(Refusal),
    #[error("refused: {}: {}", .0.reason, .0.sentence)]
    Refused(Refusal),
    #[error("the node failed ({status}): {sentence}")]
    Failed { status: u16, sentence: String },
    #[error("the node closed its change stream")]
    Closed,
    #[error("egress: {0}")]
    Egress(#[from] egress::Refused),
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Error {
        Error::Ws(Box::new(error))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Daemon<E: Context> {
    pub context: E,
    pub node: Shared<E>,
    pub descriptor: Descriptor,
    pub network: Network,
    pub identity: Vec<u8>,
    pub anchors: MarshalMailbox,
    pub summaries: Summaries<E>,
    pub logs: Logs,
    pub shutdown: watch::Sender<bool>,
    pub tips: watch::Sender<u64>,
    pub relay: relay::Relay<E>,
    pub blobs: blob::service::Service,
    halo: Arc<halo::Halo>,
    pub live: Arc<live::Relay<E>>,
    egress: egress::Station,
    key: commonware_cryptography::ed25519::PrivateKey,
    mesh: std::net::SocketAddr,
    /// Each validator's newest finalize vote, as a height (`network.rs`).
    votes: Votes,
    subscribers: Mutex<Vec<Subscriber>>,
}

/// One `/v1/changes` socket's end: the program it follows, whether it
/// asked for the keys alone, and where each block's word for it goes.
struct Subscriber {
    program: ProgramId,
    keys: bool,
    sender: mpsc::UnboundedSender<Published>,
}

/// What a `/v1/changes` socket hears of each block the node applies: what
/// the block wrote to its program, or that it wrote nothing there
/// (`Quiet`: the socket sends a ping, so a quiet program's socket is never
/// idle long enough for a proxy to cut it).
pub(crate) enum Published {
    Change(Change),
    Quiet,
}

impl<E: Context> Daemon<E> {
    pub async fn status(&self) -> Result<Status> {
        let node = self.node.lock().await;
        let tip = node.tip()?;
        Ok(Status {
            network: self.descriptor.network.clone(),
            time: self.descriptor.time,
            block_time_ms: self.descriptor.block_time_ms,
            epoch_length: self.network.epoch_length,
            height: tip.height,
            tip: tip.id,
            root: node.host().root()?,
            epoch: self.network.epoch_after(tip.height),
            identity: self.identity.clone(),
            contract: NODE_CONTRACT,
            genesis: self.descriptor.genesis_block().digest().0,
            roles: node.host().roles().clone(),
        })
    }

    /// Each block's word for `program` (`keys`: its changes' keys alone,
    /// every value left out as `None`).
    pub(crate) fn subscribe(
        &self,
        program: ProgramId,
        keys: bool,
    ) -> mpsc::UnboundedReceiver<Published> {
        let (sender, receiver) = mpsc::unbounded();
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("the subscriber lock is never poisoned");
        let stopping = *self.shutdown.borrow();
        if stopping {
            return receiver;
        }
        subscribers.push(Subscriber {
            program,
            keys,
            sender,
        });
        receiver
    }

    pub(crate) fn hang_up(&self) {
        self.subscribers
            .lock()
            .expect("the subscriber lock is never poisoned")
            .clear();
    }

    pub fn publish(&self, applied: &Applied) {
        self.live.invalidate(&applied.writes);
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("the subscriber lock is never poisoned");
        subscribers.retain(|subscriber| {
            let Some(writes) = applied.writes.programs.get(&subscriber.program) else {
                return subscriber.sender.unbounded_send(Published::Quiet).is_ok();
            };
            let value = |value: &Option<Vec<u8>>| match subscriber.keys {
                true => None,
                false => value.clone(),
            };
            let change = Change {
                height: applied.height,
                root: applied.root,
                writes: writes
                    .iter()
                    .map(|(key, written)| (key.clone(), value(written)))
                    .collect(),
            };
            subscriber
                .sender
                .unbounded_send(Published::Change(change))
                .is_ok()
        });
    }
}
