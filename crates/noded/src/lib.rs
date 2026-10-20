mod activity;
mod blocks;
mod carry;
mod changes;
mod client;
mod config;
mod cores;
mod ctx;
mod egress;
mod endpoint;
mod history;
mod link;
pub mod live;
mod logs;
mod mesh;
mod network;
mod pool;
mod relay;
mod run;
mod server;
mod transport;
mod view;
pub mod wire;
mod workspace;
mod writer;

use std::sync::Arc;

use abi::{ProgramId, Refusal};
use consensus::{MarshalMailbox, Network, Votes};
use host::Applied;
use node::{Preconfirm, Source, Taken};
use tokio::sync::watch;

pub use carry::{Carry, carry_history, restore_archives, restore_history};
pub use client::{Client, Upload};
pub use config::{
    ConsensusSettings, DEFAULT_HTTP, DEFAULT_LOG, LoadSettings, NodeConfig, Settings, permits,
};
pub use cores::{CpuList, pin_block_threads};
pub use endpoint::{Endpoint, Scheme};
pub use logs::Logs;
pub use mesh::Mesh;
pub use run::{
    HaloConfig, Listen, Listeners, LoadConfig, Running, init, invite, join, run, run_on,
};
pub use view::{View, Views};
pub use wire::{NODE_CONTRACT, Status};
pub use workspace::{Descriptor, Founding, Workspace};
pub use writer::Held;

use crate::changes::{Changes, Subscription, Wrote};

pub trait Context:
    consensus::Context + commonware_runtime::Network + commonware_runtime::Resolver
{
}

impl<E> Context for E where
    E: consensus::Context + commonware_runtime::Network + commonware_runtime::Resolver
{
}

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
    Archives(#[from] consensus::ArchivesError),
    #[error(transparent)]
    Store(#[from] fluent31::Error),
    #[error(transparent)]
    Marshal(#[from] consensus::MarshalError),
    #[error(transparent)]
    Lane(#[from] halo::Taken),
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
    #[error("the node's answer ended before every blob it counted")]
    Short,
    #[error("no block carried the frame before its anchor aged out")]
    Unlanded,
    #[error("the node stopped")]
    Stopped,
    #[error("the node's reader stopped: {0}")]
    Offloaded(String),
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
    current: Arc<view::Current<E>>,
    readers: watch::Sender<()>,
    writer: writer::Writer,
    pool: pool::Pooling<E>,
    changes: Changes,
    pub descriptor: Descriptor,
    pub network: Network,
    pub identity: Vec<u8>,
    pub anchors: MarshalMailbox,
    history: history::History,
    recent: blocks::Recent,
    activity: activity::Index,
    pub logs: Logs,
    pub shutdown: watch::Sender<bool>,
    pub tips: watch::Sender<u64>,
    pub relay: Arc<relay::Relay<E>>,
    pub blobs: blob::service::Service,
    halo: Arc<halo::Halo>,
    pub live: Arc<live::Relay<E>>,
    egress: egress::Station,
    lending: egress::Lending,
    key: commonware_cryptography::ed25519::PrivateKey,
    mesh: std::net::SocketAddr,
    /// Each validator's newest finalize vote, as a height (`network.rs`).
    votes: Votes,
}

impl<E: Context> Daemon<E> {
    pub fn view(&self) -> Result<Arc<View<E>>> {
        self.current.view()
    }

    pub fn indexed(&self) -> watch::Receiver<Option<wire::activity::Indexed>> {
        self.activity.indexed()
    }

    pub(crate) fn facts(&self) -> view::Facts<'_> {
        view::Facts {
            descriptor: &self.descriptor,
            network: &self.network,
            identity: &self.identity,
            readers: &self.readers,
        }
    }

    pub(crate) fn now(&self) -> u64 {
        millis(&self.context)
    }

    pub async fn submit(
        &self,
        frame: Vec<u8>,
        source: Source,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        let network = self.descriptor.id();
        let (frame, admitted) = offload(move || {
            let admitted = node::admissible(&frame, &network);
            (frame, admitted)
        })
        .await?;
        let submission = match admitted {
            Ok(submission) => submission,
            Err(refusal) => return Ok(Err(refusal)),
        };
        self.pool
            .submit(frame, submission, source, preconfirm)
            .await
    }

    pub async fn hold(&self) -> Result<Held> {
        self.writer.hold().await
    }

    pub async fn sync(&self, request: statesync::Request) -> Result<statesync::Response> {
        let served = self.writer.sync(request).await?;
        Ok(statesync::anchored(served, &self.anchors).await)
    }

    pub(crate) fn subscribe(&self, program: ProgramId, keys: bool) -> Subscription {
        self.changes.subscribe(program, keys)
    }

    pub(crate) async fn hang_up(&self) {
        self.changes.hang_up().await;
    }

    pub fn publish(&self, applied: &Applied) {
        self.live.invalidate(&applied.writes);
        self.changes.wrote(Wrote {
            height: applied.height,
            root: applied.root,
            writes: Arc::new(applied.writes.clone()),
        });
    }
}

pub(crate) async fn offload<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::Offloaded(error.to_string()))
}

pub(crate) fn millis(clock: &impl commonware_runtime::Clock) -> u64 {
    let since_epoch = clock
        .current()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    since_epoch.as_millis() as u64
}
