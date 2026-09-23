mod client;
mod logs;
mod mesh;
mod run;
mod server;
pub mod wire;
mod workspace;

use std::sync::{Arc, Mutex};

use abi::{ProgramId, Refusal};
use consensus::{MarshalMailbox, Network};
use futures::channel::mpsc;
use host::Applied;
use node::Node;
use tokio::sync::watch;

pub use client::Client;
pub use logs::Logs;
pub use mesh::{Mesh, PEERS_PER_SET, Reach, Tracked, tracked};
pub use run::{Listen, Running, init, invite, join, run};
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
    #[error("{}: {source}", .path.display())]
    File {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("the founding file does not parse: {0}")]
    Founding(#[from] toml::de::Error),
    #[error("not hex: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("the workspace is corrupt: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Node(#[from] node::Error),
    #[error(transparent)]
    Host(#[from] host::Error),
    #[error(transparent)]
    Sync(#[from] statesync::Error),
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
    pub logs: Logs,
    pub shutdown: watch::Sender<bool>,
    subscribers: Mutex<Vec<(ProgramId, mpsc::UnboundedSender<Change>)>>,
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
        })
    }

    pub fn subscribe(&self, program: ProgramId) -> mpsc::UnboundedReceiver<Change> {
        let (sender, receiver) = mpsc::unbounded();
        self.subscribers
            .lock()
            .expect("the subscriber lock is never poisoned")
            .push((program, sender));
        receiver
    }

    pub fn publish(&self, applied: &Applied) {
        let mut subscribers = self
            .subscribers
            .lock()
            .expect("the subscriber lock is never poisoned");
        subscribers.retain(|(program, sender)| {
            let Some(writes) = applied.writes.programs.get(program) else {
                return !sender.is_closed();
            };
            let change = Change {
                height: applied.height,
                root: applied.root,
                writes: writes
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            };
            sender.unbounded_send(change).is_ok()
        });
    }
}
