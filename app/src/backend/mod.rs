use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::chat::index::{ChatViewQuery, ChatViewReply};
// No `ChatQuery`/`ChatReply` here on purpose: every chat read in this app goes
// through `/v1/index/chat/view`, off the node's select loop. A dispatch query
// import reappearing is the signal that one crawled back onto it.
use ::chat::{ChatMsg, PostPolicy};
// this device's signing key, opened in-process by `keystore` and signing op
// frames through `::node::encode_frame` — see `rpc::Signer`.
use commonware_cryptography::{Signer as _, ed25519};
use ducktape_rpc::{Client as RpcClient, ModuleEvent, Status as NodeStatus};
use futures::{FutureExt as _, StreamExt as _};
use tokio::sync::OwnedSemaphorePermit;
use zeroize::Zeroizing;

// chat's client view model is module-owned (`chat::client`) — the rendered
// row types, the composer parsing, the optimistic merges, and the op-delta
// splices. Re-exported here for app state handlers.
pub use ::chat::client::{
    ChatChannel, ChatReader, HuddleSeat, NameDirectory, author_display, short_label,
};
// the composer's block splitter is not called by the shipping binary — only by
// the app's own test helpers, which build message rows the way a send does.
#[cfg(test)]
pub use ::chat::client::{ChatMessage, author_name, paragraph_blocks};
pub use inbox::client::{BellDelta, BellItem};
const DEFAULT_RPC: &str = "http://127.0.0.1:8844";
/// How many one-second polls the provisioning screen waits before it says the
/// node is not running and names the command that starts it.
const PROVISION_PATIENCE: u32 = 8;

#[derive(Clone, Debug, Hash, PartialEq, serde::Deserialize)]
pub struct ChatData {
    /// The switch this window answers for. Every route that moves the reader
    /// bumps `chat_generation` and stamps it here, so a room she has already
    /// clicked past cannot land on top of the one she is looking at. Thread,
    /// history, and search reads own separate compiler delivery lanes.
    pub generation: i64,
    pub channels: Vec<ChatChannel>,
    pub active_channel: String,
    pub active_channel_name: String,
    pub active_channel_archived: bool,
    /// the huddle's roster, not just its length — the faces and the tiles.
    pub huddle_roster: Vec<HuddleParticipant>,
}

#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ChatSearchHit {
    pub channel_id: String,
    pub seq: i64,
    pub root_seq: i64,
    pub author: String,
    pub text: String,
    pub meta: String,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct ChatSearchData {
    pub hits: Vec<ChatSearchHit>,
}

#[derive(Clone, Debug, Hash, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PageSearchHit {
    pub page_id: String,
    /// The title of the page the block lives in. The index's hit row carries
    /// only `page_id`, so without this join no surface could name the page a
    /// match came from; the Pages guest joins the title to each result.
    pub page_title: String,
    pub block_id: String,
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct PageSearchData {
    pub hits: Vec<PageSearchHit>,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct WorkspaceData {
    pub generation: i64,
    pub rpc: String,
    pub status: String,
    pub height: i64,
    pub channels: Vec<ChatChannel>,
    pub active_channel: String,
    pub active_channel_name: String,
    pub active_channel_archived: bool,
    pub huddle_roster: Vec<HuddleParticipant>,
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct AppError {
    pub message: String,
    pub committed: bool,
}

impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self {
            message: user_error(message),
            committed: false,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq)]
pub struct HydrationError {
    pub generation: i64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveUpdate {
    /// `ready` (topics subscribed — run the catch-up resync), `retry`
    /// (stream down, reconnecting), `chat` (one ordered bounded delta batch),
    /// `pages` (one folded delta), `resync` (this module's replay lagged —
    /// reload its slices).
    pub kind: crate::LiveKind,
    pub status: String,
    pub height: i64,
    /// the module needing a scoped resync (`kind == LiveKind::Resync`).
    pub module: String,
    /// whether the handler must reload the chat slices (`ready` after the
    /// subscribe→hydrate ordering race, or a `resync` of the chat plane).
    /// chat deltas set it false: they fold.
    pub load_chat: bool,
    /// trail 100ms so a burst of ops coalesces into one reload.
    pub debounce: bool,
    /// Ordered chat deltas. Consecutive, already-ready chat frames are
    /// published together so one network burst costs one reducer pass and one
    /// view rebuild per bounded batch, not one of each per operation.
    pub chat: Vec<ChatDelta>,
    pub bell: BellDelta,
    /// Subscription backpressure, not UI state. The next socket publication
    /// cannot be read until the app message carrying this token has
    /// finished its update and all of its clones have been dropped.
    pub(crate) permit: LivePermit,
}

#[derive(Clone, Default)]
pub(crate) struct LivePermit(Option<Arc<OwnedSemaphorePermit>>);

impl LivePermit {
    pub(crate) fn held(permit: OwnedSemaphorePermit) -> Self {
        Self(Some(Arc::new(permit)))
    }

    #[cfg(test)]
    pub(crate) fn is_held(&self) -> bool {
        self.0.is_some()
    }
}

impl std::fmt::Debug for LivePermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("LivePermit")
            .field(&self.0.is_some())
            .finish()
    }
}

impl PartialEq for LivePermit {
    fn eq(&self, _other: &Self) -> bool {
        // The permit changes scheduling only; it is not part of a publication's
        // domain value and must not perturb reducer/test equality.
        true
    }
}

impl Default for LiveUpdate {
    fn default() -> Self {
        Self {
            kind: crate::LiveKind::Retry,
            status: String::new(),
            height: 0,
            module: String::new(),
            load_chat: false,
            debounce: false,
            chat: Vec::new(),
            bell: BellDelta::default(),
            permit: LivePermit::default(),
        }
    }
}

mod agent;
mod app_dirs;
mod bell;
mod chat;
mod chat_live;
mod duck_uri;
mod explorer;
mod forge;
mod hub;
mod live;
mod load;
mod model;
mod node;
mod notify;
mod picture;
mod roster;
mod rpc;
mod search;
mod shell;
mod storage;
mod style;
pub mod update;
mod view_artifact;
pub mod view_source;

pub use agent::*;
pub use app_dirs::app_log_path;
pub(crate) use app_dirs::cache_dir;
pub use bell::*;
pub use chat::*;
pub use chat_live::*;
pub use duck_uri::*;
pub use explorer::*;
pub use forge::*;
pub use hub::*;
pub use live::*;
pub(crate) use load::*;
pub use model::*;
pub use node::*;
pub use notify::*;
pub use picture::*;
use roster::*;
pub use rpc::*;
pub use search::*;
pub use shell::*;
pub(crate) use storage::*;
pub(crate) use style::*;

#[cfg(test)]
mod tests;
