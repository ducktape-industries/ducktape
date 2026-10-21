mod activity;
mod blocks;
mod carry;
mod census;
mod changes;
mod client;
mod config;
mod cores;
mod ctx;
mod egress;
mod endpoint;
mod fill;
mod forward;
mod halo_self;
mod history;
mod link;
pub mod live;
mod logs;
pub mod machine;
mod mesh;
mod network;
mod pool;
mod query;
mod relay;
mod roll;
mod run;
mod server;
mod snapshot;
mod standing;
mod transport;
mod view;
pub mod wire;
mod workspace;
mod writer;

use std::sync::Arc;

use abi::{ProgramId, Refusal};
use consensus::{Network, Votes};
use host::Applied;
use node::{Checked, Preconfirm, Source, Taken};
use tokio::sync::watch;

pub use carry::{Carry, carry_history, restore_archives, restore_history};
pub use client::{Client, Upload};
pub use config::{
    Backend, ConsensusSettings, DEFAULT_HTTP, DEFAULT_LOG, LoadSettings, MachineEntry,
    MachineSettings, NodeConfig, Settings, memory_quantity, permits,
};
pub use cores::{CpuList, pin_block_threads};
pub use egress::lane::{LANE as EGRESS_LANE, Seating, seating, serve_box};
pub use endpoint::{Endpoint, Scheme};
pub use forward::LANE as FORWARD_LANE;
pub use logs::Logs;
pub use mesh::Mesh;
pub use roll::{Rolled, roll};
pub use run::{
    HaloConfig, Listen, Listeners, LoadConfig, Running, enroll, init, invite, join, run, run_on,
};
pub use view::{View, Views};
pub use wire::{NODE_CONTRACT, Status};
pub use workspace::{Descriptor, Founding, Stamp, Workspace};
pub use writer::Held;

use crate::changes::{Changes, Follows, Subscription, Wrote};
use crate::wire::Head;

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
    #[error("no relay node serves the egress lane")]
    Unrelayed,
    #[error("machine.key is this workspace's identity key: a machine signs with a key of its own")]
    MachineIsIdentity,
    #[error("{0}")]
    Unrolled(String),
    #[error("the forward ended before the far side's bytes did")]
    Cut,
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
    changes: Changes<E>,
    pub descriptor: Descriptor,
    pub network: Network,
    pub identity: Vec<u8>,
    holds: statesync::Holds<E>,
    holds_ended: Arc<std::sync::atomic::AtomicU64>,
    snapshot: snapshot::Served,
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
    answered: tokio::sync::mpsc::UnboundedSender<server::Answered>,
    queried: tokio::sync::mpsc::UnboundedSender<census::Queried>,
    key: commonware_cryptography::ed25519::PrivateKey,
    mesh: std::net::SocketAddr,
    /// Each validator's newest finalize vote, as a height (`network.rs`).
    votes: Votes,
    halo_self: watch::Sender<wire::HaloSelf>,
    forward: forward::Forwarding,
}

pub struct HeldHead {
    pub height: u64,
    pub hold: statesync::Holding,
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
        let checked = match self.admit(frame).await? {
            Ok(checked) => checked,
            Err(refusal) => return Ok(Err(refusal)),
        };
        self.take(checked, source, preconfirm).await
    }

    pub(crate) async fn admit(
        &self,
        frame: Vec<u8>,
    ) -> Result<std::result::Result<Checked, Refusal>> {
        let network = self.descriptor.id();
        offload(move || node::admissible(frame, &network)).await
    }

    pub(crate) async fn take(
        &self,
        checked: Checked,
        source: Source,
        preconfirm: Preconfirm,
    ) -> Result<std::result::Result<Taken, Refusal>> {
        self.pool.submit(checked, source, preconfirm).await
    }

    pub async fn hold(&self) -> Result<Held> {
        self.writer.hold().await
    }

    pub async fn held_head(&self) -> Result<(statesync::Response, Option<HeldHead>)> {
        let view = self.view()?;
        let height = view.sync().tip().height;
        let (served, hold) = statesync::held(view.sync().clone(), &self.holds);
        let held = hold.map(|hold| HeldHead {
            height,
            hold: Box::new(hold),
        });
        Ok((statesync::anchored(served, &**view.sync()).await, held))
    }

    pub async fn sync(&self, request: statesync::Request) -> Result<statesync::Response> {
        let view = self.view()?;
        let served = statesync::serve(view.sync(), &self.holds, request).await;
        Ok(statesync::anchored(served, &**view.sync()).await)
    }

    pub(crate) fn subscribe(&self, program: ProgramId, keys: bool) -> Subscription {
        self.changes.subscribe(Follows::Program { program, keys })
    }

    pub(crate) fn subscribe_heads(&self) -> Subscription {
        self.changes.subscribe(Follows::Heads)
    }

    pub(crate) async fn hang_up(&self) {
        self.changes.hang_up().await;
    }

    pub fn publish(&self, applied: &Applied, head: Head) {
        self.live.invalidate(&applied.writes);
        let view = self
            .current
            .view()
            .ok()
            .filter(|view| view.tip().height == applied.height);
        self.changes.wrote(Wrote {
            head,
            root: applied.root,
            writes: Arc::new(applied.writes.clone()),
            places: applied.places.clone(),
            view,
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
