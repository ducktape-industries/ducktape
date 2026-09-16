//! the daemon's client-facing surface: json wire types, the node-actor command
//! channel, and the axum router.
//!
//! the split matters: `host::Host` is deliberately non-Send (single-threaded by
//! design), so http handlers never touch it. they send a [`NodeCommand`] over a
//! runtime-agnostic futures mpsc channel to whichever actor owns the host — the
//! real one lives in `main.rs` on a commonware tokio runner; router tests drive
//! a fake actor on plain tokio. payloads stay opaque json: a submit/query body
//! carries the module's own `*Msg`/`*Query` enum as a json value, encoded to the
//! exact bytes the modules' crate-root `encode_*` helpers would produce
//! (`serde_json::to_vec`), so the daemon needs no per-module knowledge —
//! with ONE deliberate exception: the op-receipt blob lane. receipt bytes
//! never transit consensus (no op carries them), so POST `/v1/files/blob` and
//! GET `/v1/files/blob/{digest}` bypass the actor entirely and talk straight
//! to the node-local [`crate::blobs::BlobHandle`] forge and the block loop share.
//!
//! lifecycle is part of the surface: `/v1/status` carries the daemon's build
//! version (so a newer app can spot a stale orphan), and POST `/v1/admin/shutdown`
//! asks the process to exit gracefully — the managing app has no pid, only
//! this port.

// the owner-gated control namespace: `/v1/admin/*` on the same
// listener, PoP-gated to the node owner. shutdown + module-code moved here off
// the unauthenticated public surface.
pub mod admin;
pub use admin::{AdminConfig, AdminExposure};

// the per-request signature every MUTATING `/v1` route now carries, and the
// verifier both it and `/v1/admin/*` share. reads stay open.
pub mod signed_req;
pub use signed_req::{SignedBy, WriteRefusal};

pub mod blobs;
// the pieces every process needs AROUND the composer: the on-disk component
// bundle (naming, hashing, a code source over it), the qmdb store source, and
// the `Host` finisher.
pub mod bundle;
// the ONE module composer every host in the workspace builds its module set
// through: deployment hashes + a code source + a store source.
pub mod compose;
pub mod log;
pub mod stream;
pub use stream::{
    BlockWake, ClientMsg, LogRing, RunOutputEvent, RunOutputRegistry, RunStream, ServerFrame,
    StreamErrorCode, StreamHub, StreamOpRow, StreamOrigin, StreamOriginKind, TailItem,
};
// the workspace RPC (`/v1/fs/workspaces`) and its actor-lane `NodeApi` adapter.
// crate-internal: the router registers the handlers and the adapter is used only
// by the workspace handlers — nothing outside the crate touches either.
mod actor_api;
mod workspaces;
// the REAL portable-agent-run provisioner (NodedProvisioner + agent_runs_root,
// the D7 root). public so BOTH node binaries can build one and wire it into
// their DispatchPool constructor.
pub mod agent_provision;
// Pages presence session/control types.
mod call;
pub use call::{
    PageCursor, PresenceClientControl, PresenceControlIn, PresenceControlOut, PresenceLane,
    PresenceParams, PresenceServerControl, PresenceSession, PresenceSessionRequest,
};
// the gateway lane: signed-route proxying + the isolated browser-gateway
// origin (`gateway_http` because the `gateway` crate is a dependency).
mod gateway_http;
pub mod gateway_ws_token;
pub mod origin_guard;
pub use gateway_http::{
    GatewayBody, GatewayFailure, GatewayJob, GatewayLane, GatewayProxyReply, GatewayProxyRequest,
    GatewayResponse, GatewayWsMsg, PROXY_REPLY_TIMEOUT, collect_body, gateway_browser_router,
    gateway_caller_account, serve_browser_gateway,
};
// the node-actor command lane and the router's shared state handle.
mod handle;
pub use handle::{
    NetstackSwapRequest, NetstackSwapper, NodeCommand, NodeHandle, PeersStanding, StatusCell,
};

mod module_code;
pub mod node_work;
pub use module_code::{
    CODE_KIND_MODULE, CodePeerReceipt, CodeStageLane, CodeStageRequest, MAX_MODULE_ARTIFACT_BYTES,
};
pub mod run_control;
// the node ↔ agent-daemon link: the collaboration messaging bus, and the 0600
// workspace secret the gated ws topics stand on. public so `main.rs` can build
// it and wire it onto the handle.
pub mod service_link;
pub use service_link::{AttachGuard, ServiceLink};

/// the volatile catalog of service daemons signaling presence to this node.
pub mod services;

/// A service daemon's handle on the node it serves: the `/v1` twin of the
/// in-process `NodeCommand` actor lane. See [`node_link::NodeLink`].
pub mod node_link;
// the derived-index tier: store construction, boundary stamps, /v1/index/* +
// /v1/blocks.
mod index;
pub use index::{
    BlocksParams, FOLDED_HEADER, IndexScanParams, converge_host_modules, index_block_ops,
    index_host_modules, index_origin, open_index_store, stale_modules, stamp_stale_modules,
};
// the ducktape_* Prometheus series + GET /metrics.
mod metrics;
pub use metrics::{NodeMetrics, spawn_store_footprint_sampler};
// the block-projection seam: RootOp assembly + explorer-row bytes, shared by
// the validator drain, the replica park loop, and (as later tasks adopt it) the
// noded submit lane and simnode. One row shape, pinned by a golden test.
pub mod projection;
pub use projection::{BlockProjection, NOP_TARGET, project_block, project_root_op};

// the direct-peer projection: `GET /v1/peers` and the local rpc's `peers`
// cmd both answer with its [`peers::PeersView`], parsed from the lane's own
// metrics exposition.
pub mod peers;

// the in-process daemon testkit (a real Host + router on loopback threads) for
// e2e harnesses. dev-only: gated so the shipping node never compiles it.
#[cfg(feature = "testkit")]
pub mod testkit;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use commonware_cryptography::Signer as _;
use futures::channel::oneshot;
use sdk::StateRoot;
use serde::{Deserialize, Serialize};
use workspace_config::{DEFAULT_INVITE_TTL_DAYS, INVITE_TTL_DAYS};

use crate::call::presence_ws;
use crate::gateway_http::{gateway_browser_base, gateway_proxy};
use crate::index::{blocks, index_ops, index_scan, index_status, index_view};
use crate::metrics::metrics;

/// one finalized block, as reported to clients (http response + ws frame).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockSummary {
    pub height: u64,
    pub root_hash: String,
}

/// the `/v1/submit` reply: the block that INCLUDED the caller's op, plus the
/// op's content address — sha256 of the exact payload bytes the host committed.
/// the bytes are staged in the node-local blob store under that digest, so
/// `GET /v1/files/blob/{op_hash}` serves them back: the hash is addressable,
/// not just informational.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitReceipt {
    pub height: u64,
    pub root_hash: String,
    pub op_hash: String,
}

/// one dispatch in a block's drain — the wire twin of `host::DispatchRecord`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DispatchInfo {
    /// the module dispatched this step.
    pub module: String,
    /// what triggered it: `"external"`, `"external:<name>"`, `"system"`, or
    /// `"module:<id>"` for a follow-up.
    pub origin: String,
    /// follow-up `Msg`s this dispatch emitted (its causal fan-out).
    pub emitted_msgs: usize,
    /// observability `Event`s this dispatch emitted.
    pub emitted_events: usize,
}

/// cap on the payload-preview characters a block record carries.
pub(crate) const PAYLOAD_PREVIEW_MAX: usize = 1024;

/// how a block's op landed, as the explorer surface reports it. only
/// journaled outcomes appear (a frame discarded at an epoch cutover is
/// dropped before decoding, so there are no contents to show): an applied op
/// mutated state; a rejected op finalized but rolled back — a failed tx.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockDisposition {
    Applied,
    Rejected,
}

/// one member op inside a finalized block — the per-op detail the explorer
/// fans out over. a block now AGGREGATES the txs from its window, so it carries
/// a vector of these in agreed (applied) order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootOp {
    /// hex of this op's authenticated author origin (the frame's VERIFIED
    /// signer), or `"system"` / `"module:<id>"` for a non-external origin. on
    /// the embedded daemon's frameless lane this is the SUBMITTER's origin
    /// bytes instead (unverified — that lane authenticates nothing).
    pub proposer: String,
    /// how this op landed: `applied` mutated state; `rejected` finalized but
    /// rolled back (a failed tx). deterministic on every validator.
    pub disposition: BlockDisposition,
    /// this op's target module.
    pub target: String,
    /// this op's dispatch trace, in drain order (empty for a rejected op).
    pub operations: Vec<DispatchInfo>,
    /// best-effort utf-8 preview of this op's payload (module `*Msg` json on
    /// this lane), capped at [`PAYLOAD_PREVIEW_MAX`] chars.
    pub payload: String,
    /// hex of this op's payload content address — sha256 of the exact bytes the
    /// host committed, staged in the node-local blob store so
    /// `GET /v1/files/blob/{op_hash}` serves the full bytes back.
    pub op_hash: String,
}

/// one non-empty finalized block, as the explorer reads it: the block's
/// consensus coordinates (height, frame content hash, post-block root-hash) and
/// the member ops it AGGREGATED, each with its deterministic dispatch trace.
/// stored as the block's row in the index store's blocks database
/// ([`indexer::BlockOps::record`]) and served by `GET /v1/blocks`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockRecord {
    pub height: u64,
    /// hex of the block's content address — the block's hash on this surface.
    /// empty on the embedded daemon's lane: nothing is framed or signed there,
    /// so the field stays honest rather than carrying a fabricated digest.
    pub hash: String,
    /// hex of the composed root-hash after this block settled — the commit.
    pub commit_hash: String,
    /// the member ops this block aggregated, in agreed (applied) order. empty
    /// for an idle/nop block (nothing but the heartbeat filler).
    pub ops: Vec<RootOp>,
}

/// the explorer's wire rendering of one dispatch. `Origin::External` renders
/// as plain `"external"`: the block-level `proposer` field already carries
/// the key, and raw ed25519 bytes are not utf-8 (the `external:<name>`
/// convention assumes the embedded daemon's readable names).
impl From<&host::DispatchRecord> for DispatchInfo {
    fn from(record: &host::DispatchRecord) -> Self {
        DispatchInfo {
            module: record.module.clone(),
            origin: match &record.origin {
                sdk::Origin::External(_) => "external".to_string(),
                sdk::Origin::Module(id) => format!("module:{id}"),
                sdk::Origin::Program(account) => format!("acct:{account}"),
                sdk::Origin::System => "system".to_string(),
            },
            emitted_msgs: record.emitted_msgs,
            emitted_events: record.emitted_events,
        }
    }
}

/// best-effort utf-8 preview of an op payload, capped at
/// [`PAYLOAD_PREVIEW_MAX`] chars. binary bytes render lossily — payloads on
/// this lane are module `*Msg` json, so the common case is readable.
///
/// the decode is bounded BEFORE the lossy conversion: this runs on the
/// consensus drain for every op, and a 1 MiB binary chunk (a dropped photo)
/// lossy-decoded whole allocates ~3 MiB and scans it all to keep 1024 chars.
/// 4 bytes/char ceilings the prefix that can ever render.
pub fn payload_preview(payload: &[u8]) -> String {
    const DECODE_BYTES: usize = 4 * PAYLOAD_PREVIEW_MAX;
    let clipped = payload.len() > DECODE_BYTES;
    let head = &payload[..payload.len().min(DECODE_BYTES)];
    let text = String::from_utf8_lossy(head);
    match text.char_indices().nth(PAYLOAD_PREVIEW_MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None if clipped => format!("{text}…"),
        None => text.into_owned(),
    }
}

/// encode a [`BlockRecord`] as its stored index row ([`indexer::BlockOps::record`]).
/// both binaries feed rows through this one seam so `GET /v1/blocks` reads a
/// single shape regardless of which lane wrote it.
pub fn block_row(record: &BlockRecord) -> Vec<u8> {
    serde_json::to_vec(record).expect("a plain record struct serializes")
}

/// The shape of this build's app-facing surface, as one integer: the `/v1`
/// routes and their bodies, the ws topics and their frames, the view-props
/// JSON a module view is handed, and the `duck://` URI grammar. Served on
/// `GET /v1/status` and the ws `status` topic as [`NodeStatus::contract`].
///
/// Bumped in the PR that changes ANY of those — [`NODE_CONTRACT_SURFACE`] is
/// the pin that makes forgetting fail the test lane. The desktop app carries
/// its own copy (`EXPECTED_NODE_CONTRACT`) and opens a console only on
/// EQUALITY: never a tolerance window, never "N-1 still works" — that would be
/// the compat the repository forbids. Nothing on the node reads it, no peer
/// sees it, and no code branches on its value; the app alone compares.
pub const NODE_CONTRACT: u32 = 6;

/// The surface [`NODE_CONTRACT`] names, fingerprinted: FNV-1a over the sorted
/// `/v1` route paths of `lib.rs` + `admin.rs` and the ws topic/prefix names
/// of `stream.rs`. The `contract_lint` test recomputes it from source; when
/// they differ, the surface changed — bump [`NODE_CONTRACT`] and the app's
/// `EXPECTED_NODE_CONTRACT` together, then repin this to the value the
/// failing assertion prints. Repinning WITHOUT the bump is the defect the
/// test exists to catch.
pub const NODE_CONTRACT_SURFACE: u64 = 0x3ab4_2d02_809c_f369;

/// the status projection: daemon build version, global root-hash, and each
/// registered module's root. `Default` is the pre-first-publish snapshot in
/// [`StatusCell`] — zeroed boundary facts are the honest answer before any
/// boundary is served; the contract number is this build's even then.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeStatus {
    /// [`NODE_CONTRACT`], always. Every publisher writes the constant, the
    /// way `version` is always `CARGO_PKG_VERSION`: the number is a fact
    /// about the binary, not about the boundary being published.
    pub contract: u32,
    pub version: String,
    pub root_hash: String,
    pub height: u64,
    /// the `Env` clock every module compares `expires_at` against, stamped
    /// from `height` under this node's `ConsensusTimePolicy` — a block height
    /// on the validator/replica lanes, a millisecond epoch on the sim lane.
    /// a client minting a TTL-bounded consent (identity's `AddKey`) MUST read
    /// this, never `height`: the two units diverge on simnode.
    pub consensus_time: u64,
    /// which unit [`Self::consensus_time`] is expressed in, so a client can
    /// scale a TTL window correctly regardless of which lane it is talking
    /// to. defaults to `Height` (the validator/replica lanes' only policy).
    pub consensus_time_unit: ConsensusTimeUnit,
    pub modules: Vec<ModuleStatus>,
    /// this node's mesh identity (hex ed25519 key) — what a client stamps
    /// into ops that route peer traffic to it (chat's `JoinHuddle.node`).
    /// empty on daemons with no mesh identity (the embedded local daemon).
    pub public_key: String,
    /// the chain id every chain-scoped USER proof is minted for — an identity
    /// `AddKey` consent, a gateway route statement. Wired once at boot into
    /// the [`StatusCell`] (`wire_chain_id`) and overlaid on every read, so a
    /// boundary publish never has to carry it; empty on a daemon that serves
    /// no chain (simnode, the embedded local daemon).
    pub chain_id: String,
    /// Node-owned operational state. This is the stable, role-aware facade for
    /// operators; dependency-specific consensus and transport metrics remain
    /// available on `/metrics` for deeper diagnosis.
    pub operations: OperationalStatus,
}

impl Default for NodeStatus {
    fn default() -> Self {
        Self {
            contract: NODE_CONTRACT,
            version: String::new(),
            root_hash: String::new(),
            height: 0,
            consensus_time: 0,
            consensus_time_unit: ConsensusTimeUnit::default(),
            modules: Vec::new(),
            public_key: String::new(),
            chain_id: String::new(),
            operations: OperationalStatus::default(),
        }
    }
}

/// The job this process is currently performing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    /// Single-writer embedded daemon; no mesh or consensus participation.
    Local,
    Validator,
    Resident,
    SyncOnly,
    /// Used only until the full node has selected its role during boot.
    #[default]
    Unknown,
}

impl NodeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Validator => "validator",
            Self::Resident => "resident",
            Self::SyncOnly => "sync_only",
            Self::Unknown => "unknown",
        }
    }
}

/// The role-independent lifecycle phase. A role says what the node is; a
/// phase says what it is doing now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodePhase {
    #[default]
    Starting,
    Recovering,
    Joining,
    Syncing,
    Validating,
    Serving,
    Draining,
    Halted,
}

impl NodePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Recovering => "recovering",
            Self::Joining => "joining",
            Self::Syncing => "syncing",
            Self::Validating => "validating",
            Self::Serving => "serving",
            Self::Draining => "draining",
            Self::Halted => "halted",
        }
    }
}

/// Stable operational projection shared by `/v1/status` and the
/// `ducktape_*` metrics. Optional sections are absent when they do not apply to
/// the selected role, rather than being filled with misleading zeroes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationalStatus {
    pub role: NodeRole,
    pub phase: NodePhase,
    /// Unix seconds when `phase` last changed.
    pub phase_since: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_finalized_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consensus: Option<ConsensusOperationalStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncOperationalStatus>,
    pub storage: StorageOperationalStatus,
    /// Which machine the reachability plane runs on, and how the last swap
    /// went. Absent on a node with no reachability plane at all (no
    /// `wireguard_listen`) — there is no backend to name there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub netstack: Option<NetstackOperationalStatus>,
}

/// The netstack plane's operator-visible standing: the backend name
/// ([`NetstackBackend::name`] on the executor side) and the outcome of the last
/// swap this process performed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetstackOperationalStatus {
    /// Actual execution: `starting`, `guest`, `stopped`, or `failed`.
    pub backend: String,
    /// SHA-256 of the component actually running, absent when no guest runs.
    pub code_hash: Option<String>,
    /// `null` until this process swaps once. A refused swap is recorded here
    /// AND leaves `backend` unchanged: the running machine continues.
    pub last_swap: Option<NetstackSwap>,
}

/// One swap attempt's outcome — the projection half of the
/// `netstack_backend_swapped` / `netstack_backend_swap_refused` log events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetstackSwap {
    pub outcome: NetstackSwapOutcome,
    /// why a refused swap was refused; absent on a swap that took.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// the local block height when the plane answered.
    pub at_height: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetstackSwapOutcome {
    Swapped,
    Refused,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsensusOperationalStatus {
    pub epoch: u64,
    pub view: u64,
    pub validators: u64,
    pub quorum: u64,
    /// Current validators this node can use, including itself when it is a
    /// member. This makes the number directly comparable with `quorum`.
    pub reachable_validators: u64,
    pub pending_ops: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncOperationalStatus {
    /// Source identity is useful in status and logs but intentionally not a
    /// metric label (peer identities are unbounded cardinality).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub target_height: u64,
    pub applied_height: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<u64>,
    pub retries: u64,
    pub failures: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageOperationalStatus {
    pub checkpoint_height: u64,
    pub index_poisoned: bool,
    pub indexes: Vec<IndexOperationalStatus>,
    /// what the node-local retained stores hold on disk. NOTHING PRUNES THEM
    /// (#1309): every applied op payload is written to the blobstore under its
    /// digest AND to an indexer op row, and neither is ever removed, so these
    /// numbers only climb. Sampled on a slow cadence off the node's own task
    /// — see `metrics::spawn_store_footprint_sampler`.
    pub stores: Vec<StoreOperationalStatus>,
}

/// one retained node-local store's on-disk footprint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreOperationalStatus {
    /// the bounded store name — `blobstore` or `index`.
    pub store: String,
    pub bytes: u64,
    /// files on disk — the same number `ducktape_store_files` carries. the
    /// blobstore is ONE FLAT DIRECTORY of op-payload blobs, so this is also
    /// the count an operator's `ls` has to survive.
    pub files: u64,
}

/// the unit a `NodeStatus::consensus_time` reading is in — mirrors
/// [`node::ConsensusTimePolicy`]'s two arms, so a status client never needs
/// the kernel crate just to scale a TTL correctly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsensusTimeUnit {
    /// `consensus_time` IS the block height (the validator/replica lanes'
    /// only policy); one unit is roughly one second at a one-block-per-second
    /// heartbeat.
    #[default]
    Height,
    /// `consensus_time` is a millisecond epoch clock (the sim lane); one unit
    /// is one millisecond.
    Millis,
}

impl From<node::ConsensusTimePolicy> for ConsensusTimeUnit {
    fn from(policy: node::ConsensusTimePolicy) -> Self {
        match policy {
            node::ConsensusTimePolicy::HeightIsTime => Self::Height,
            node::ConsensusTimePolicy::Epoch { .. } => Self::Millis,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexOperationalStatus {
    pub module: String,
    pub applied_height: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleStatus {
    pub id: String,
    pub root: String,
    pub category: ModuleCategory,
}

/// A module's presentation category — how the app's Modules view groups the
/// registered set. This is catalog metadata the status projection attaches by
/// id; it is not part of a module's consensus identity (that stays `id` +
/// `root`) and never enters the root-hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleCategory {
    Workspace,
    Developer,
    Automation,
    System,
}

impl ModuleCategory {
    /// The category a module id belongs to. Ids not listed here —
    /// infrastructure and internal modules (files, saga, identity, kv,
    /// valset, governance, …) — fall to `System`, so a new
    /// or unknown module always groups sensibly rather than breaking the view.
    pub fn of(id: &str) -> Self {
        match id {
            "chat" | "tasks" | "inbox" | "pages" => Self::Workspace,
            "forge" | "agent" => Self::Developer,
            "automations" => Self::Automation,
            _ => Self::System,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub target: String,
    /// the module's `*Msg` enum as a json value — encoded verbatim into `Msg.payload`.
    pub payload: serde_json::Value,
    /// A transport prerequisite, independent of the module payload.
    #[serde(default, deserialize_with = "deserialize_required_blob")]
    pub required_blob: Option<[u8; 32]>,
    /// the submitter identity stamped into `Origin::External` on a daemon that
    /// honours it (the embedded one, and simnode).
    ///
    /// THIS IS A CLAIM, NOT AUTHENTICATION. It is honoured only on a request
    /// the OPERATOR CREDENTIAL admitted — a local daemon speaking for the node
    /// it lives beside, which is the only caller left that can reach this
    /// field. A request that signed acts as its verified key instead, and no
    /// caller can present neither credential at all any more.
    ///
    /// It still carries no authorship consensus can check: `bin/node` throws
    /// the claim away and re-signs with the node key. The lane that does is
    /// `/v1/submit/frame`, whose origin IS its verified signer.
    ///
    /// Deleting the field is a small change HERE and a large one everywhere
    /// else: the simnode, embedded-daemon and MCP harnesses use it to model
    /// distinct external authors across ~270 call sites, and their replacement
    /// is a per-persona key signing a frame (simnode's harness already has
    /// `submit_frame`). That migration is the work; the field is only its
    /// symptom.
    #[serde(default)]
    pub origin: Option<String>,
}

fn deserialize_required_blob<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u8; 32]>, D::Error> {
    let digest = String::deserialize(deserializer)?;
    duckfs_core::from_hex_32(&digest).map(Some).ok_or_else(|| {
        serde::de::Error::custom("required_blob must be 64 lowercase hexadecimal characters")
    })
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitBlob {
    #[serde(default, deserialize_with = "deserialize_required_blob")]
    required_blob: Option<[u8; 32]>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    pub target: String,
    /// the module's `*Query` enum as a json value.
    pub query: serde_json::Value,
}

/// hex-encode a state root for the wire (stable, greppable, json-friendly).
pub fn hex_root(root: &StateRoot) -> String {
    hex_bytes(&root.0)
}

/// hex-encode arbitrary wire bytes (frame content hashes, proposer keys).
/// a thin wrapper over [`duckfs_core::to_hex`] — one hex implementation; the
/// pub name stays because simnode and bin/node consume it.
pub fn hex_bytes(bytes: &[u8]) -> String {
    duckfs_core::to_hex(bytes)
}

/// the ONE funnel every HTTP rejection flows through — 403 origin-guard, 413 body
/// cap, 409 conflict, 503 no-mesh, every module 400. three lines light up the whole
/// surface.
///
/// 4xx is `debug` ON PURPOSE: `gateway_http.rs`'s duck:// browse fallback proxies
/// UNTRUSTED pages' fetches through this same funnel, so an unconditional `warn!`
/// here is a log-ring DoS any page could drive. Turn it on when you care:
///     ducktape node log-filter 'info,ducktape::http=debug'
///
/// 5xx is LATCHED for the same reason, and it is not hypothetical: the gateway
/// browse proxy maps a slow/dead publisher to a 502 (`gateway_failure_response`),
/// so a page whose script re-fetches a failing subresource in a loop mints one
/// line per request — enough to evict the whole 4096-line ring. First occurrence,
/// then every 50th, carrying `occurrences`; a real outage is still visible on the
/// first line, and the counter is what says "still broken" rather than "flapped".
///
/// NEVER log the URI: `/.duck/ws/{token}` carries a capability token IN THE PATH,
/// and the ring is streamed to the webview.
pub(crate) fn error_response(status: StatusCode, message: &str) -> Response {
    if status.is_server_error() {
        static SERVER_ERRORS: crate::log::Latch = crate::log::Latch::new(50);
        // keyed by class, not by message: an attacker-supplied path can vary the
        // message, and a per-message key would let them mint an unbounded number of
        // "first occurrences" — re-opening the exact hole this latch closes.
        if let Some(occurrences) = SERVER_ERRORS.hit("server_error") {
            tracing::warn!(
                target: "ducktape::http",
                status = status.as_u16(),
                message,
                occurrences,
                "request failed"
            );
        }
    } else {
        tracing::debug!(target: "ducktape::http", status = status.as_u16(), message, "request refused");
    }
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

/// the actor dropped the reply oneshot — it panicked or shut down mid-request.
fn actor_gone() -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "node actor dropped the reply",
    )
}

pub fn router(handle: NodeHandle) -> Router {
    // the PUBLIC (data) surface — query/submit + reads, any account with
    // standing. control ops (shutdown, module-code staging) are NOT here: they
    // live on the owner-gated `/v1/admin/*` namespace merged below.
    let public = Router::new()
        .route("/v1/submit", post(submit))
        // the AUTHENTICATED submit lane: raw signed frame bytes in, the same
        // receipt out. distinct from `/v1/submit` above, whose `origin` is a
        // caller-supplied string.
        .route("/v1/submit/frame", post(submit_frame))
        .route("/v1/submit/raw/{target}", post(submit_raw).layer(DefaultBodyLimit::max(node::MAX_PAYLOAD_BYTES)))
        .route("/v1/query", post(query))
        // the AUTHENTICATED read lane, to `/v1/query` what `/v1/submit/frame`
        // is to `/v1/submit`: the caller's own proof decides who is asking, so
        // a module can serve content it would refuse an anonymous reader.
        .route("/v1/query/reader", post(query_as_reader))
        .route("/v1/status", get(status))
        .route("/v1/peers", get(peers))
        .route("/v1/blocks", get(blocks))
        // the derived read-model tier: snapshot reads of the per-module
        // fluent31 indexes the actor materializes as blocks commit.
        .route("/v1/index/status", get(index_status))
        .route("/v1/index/{module}/ops", get(index_ops))
        .route("/v1/index/{module}/scan", get(index_scan))
        // the module's OWN endpoint on the derived tier: an opaque
        // module-defined view request, answered by its registered mapper.
        .route("/v1/index/{module}/view", post(index_view))
        // Prometheus scrape convention: root `/metrics`, not under `/v1`.
        .route("/metrics", get(metrics))
        .route("/v1/log-filter", post(log_filter))
        .route("/v1/huddle/node-proof", post(huddle_node_proof))
        .route("/v1/ws", get(ws))
        .route("/v1/presence/ws", get(presence_ws))
        .route(
            "/v1/gateway/proxy",
            post(gateway_proxy).layer(DefaultBodyLimit::max(
                gateway_http::JSON_LANE_REQUEST_BYTES * 2 + gateway::MAX_PROXY_HEAD_BYTES,
            )),
        )
        .route(
            "/v1/gateway/operator",
            post(gateway_http::gateway_operator_proxy)
                .get(gateway_http::gateway_operator_stream)
                .layer(DefaultBodyLimit::max(
                    gateway_http::JSON_LANE_REQUEST_BYTES * 2 + gateway::MAX_PROXY_HEAD_BYTES,
                )),
        )
        .route("/v1/gateway/browser", get(gateway_browser_base))
        .route(
            "/v1/gateway/stream",
            get(gateway_http::gateway_native_stream),
        )
        .route(
            "/v1/files/blob",
            // NO body limit, and the one route that has none: `put_blob`
            // streams to disk, and the bytes arriving here are a git push's
            // packfile — capping them caps what anyone may push. The json
            // routes keep axum's (smaller) default limit.
            post(put_blob).layer(DefaultBodyLimit::disable()),
        )
        .route("/v1/files/blob/{digest}", get(get_blob))
        // ---- duckfs workspace RPC (the jobs/sandbox seam) ----
        // managed checkouts under the injected root: create, commit (409 on a
        // structured conflict), delete. `None` root → 503.
        // ---- admission ----
        // minting an invite is a WRITE to this node's own descriptor and a read
        // of its persisted mesh — the daemon that owns those files does it.
        .route("/v1/invite", post(mint_invite))
        // ---- run control (node-local, off-chain) ----
        .route("/v1/run-control", post(run_control::control))
        // ---- service signaling (node-local, off-chain, volatile) ----
        // a local service daemon says hello; the entry ages out on its own
        // TTL. Presence only — enablement lives in the workspace's
        // services.toml and is never inferred from a hello (see services.rs).
        .route("/v1/services/hello", post(services::hello))
        .route("/v1/services", get(services::list))
        .route("/v1/fs/workspaces", post(workspaces::create_workspace))
        .route(
            "/v1/fs/workspaces/{id}/commit",
            post(workspaces::commit_workspace),
        )
        .route(
            "/v1/fs/workspaces/{id}",
            delete(workspaces::delete_workspace),
        );

    // EVERY mutating route above carries a credential; the reads do not. WHICH
    // credential is `signed_req::Lane::authority`'s call: a module-bound write
    // takes any acting key (the module decides), a node-level one takes this
    // node's operator.
    // `route_layer`, NOT `layer`: a layer would also wrap the fallback, and an
    // unmatched path must 404 rather than be told it needs a signature.
    // `signed_req::lane_of` is the whole table — a new mutating route is added
    // THERE, never gated at its own call site.
    let public = public.route_layer(axum::middleware::from_fn_with_state(
        handle.clone(),
        signed_req::signed_write_guard,
    )).route_layer(axum::middleware::from_fn_with_state(
        std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
        limit_blob_uploads,
    ));
    // the owner-gated `/v1/admin/*` namespace — merged only when exposure is
    // enabled, so `Disabled` leaves the control surface simply ABSENT (a 404),
    // not a gated-but-present route. its own PoP middleware is baked in.
    let app = if handle.admin.exposure.enabled() {
        public.merge(admin::admin_router(handle.clone()))
    } else {
        public
    };
    app
        // The public data plane forges account-signed consensus ops, reads all
        // state, writes the filesystem and pushes git. The trusted console is the
        // ONLY web page allowed to reach it: the guard refuses any other browser
        // origin, and the matching CORS allowlist stops a page reading a response
        // even on a request that carries no Origin at all. See `origin_guard`.
        // (The admin namespace inherits this outer origin guard AND its own PoP
        // gate — defense in depth.)
        .layer(axum::middleware::from_fn(origin_guard::guard))
        .layer(origin_guard::cors())
        .with_state(handle)
}

/// the fallback submitter identity when a client sends no `origin`.
pub const DEFAULT_ORIGIN: &str = "noded";

/// the EMBEDDED daemon's executing identity: the origin its dispatch pool
/// claims a run under, so an Accept claim records THIS key as the run's
/// assignee and every op the daemon submits on the run's behalf (the oracle
/// result, the agent session it opens) matches the lease-holder consensus
/// committed. the real node has a keypair for this and signs; the embedded
/// daemon has only its trusted-client origin string, so it must be ONE string
/// — the binary's actor loop, its oracle pool, and the provisioner all name it
/// here rather than each inventing a spelling. 32 bytes: `Accept` gates on
/// valset standing (and, for a tagged saga, an announced capability), the
/// same shape a real node's ed25519 key has — a scenario that needs this
/// identity to actually claim a saga registers it via `--with-valset`.
pub const ORACLE_ORIGIN: &[u8] = &[b'o'; 32];

/// the network name BOTH single-writer daemons compose under: the composer
/// binds it into the identity and gateway guests' genesis `__config`, and
/// `/v1/status` serves it back. ONE value across noded and simnode — their
/// genesis roots are comparable only while they share it, and a client that
/// signs an identity consent reads the chain id from status, so a status
/// disagreeing with the bindings would mint signatures nothing accepts.
pub const LOCAL_CHAIN_ID: &str = "local";

/// POST /v1/submit — the FRAMELESS lane.
///
/// It is how the node's OWN local daemons submit ops that must carry the NODE's
/// identity: a capability announce (`capability` keys the registry on the
/// submitting node), a compute lease heartbeat and bid (the lease-holder IS the
/// node), an agent run bind (`runs` checks the committed assignee). No user key
/// would be right for any of those, so requiring a user signature here would
/// name the WRONG actor — which is why the gate takes the OPERATOR CREDENTIAL
/// as well (`signed_req`), and those daemons present it. Nothing else gets in:
/// leaving the route open to anyone who could dial the port is what let a
/// sandbox guest, or any peer on a widened `http_listen`, forge an op under
/// this node's own consensus key.
///
/// `signed_req` gates this route at `Authority::Operator`: the only PoP that
/// clears it is a signature by [`crate::AdminConfig::owner_key`] (the operator's
/// own key), so a signed caller here has already proven it IS the operator, and
/// that verified signer overrides the caller-supplied [`SubmitRequest::origin`],
/// which is a claim (see there) — never a self-chosen key, which the gate never
/// lets reach this handler at all (#1808).
async fn submit(
    State(handle): State<NodeHandle>,
    signed: Option<axum::Extension<SignedBy>>,
    Json(req): Json<SubmitRequest>,
) -> Response {
    let payload = serde_json::to_vec(&req.payload).expect("a decoded json value re-serializes");
    // a verified signer is an IDENTITY; `origin` is only ever a claim, so the
    // claim never wins. empty string falls back too — chat rejects empty
    // external authors.
    let origin = match signed.as_deref() {
        Some(SignedBy(key)) => key.clone(),
        None => req
            .origin
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| DEFAULT_ORIGIN.to_string())
            .into_bytes(),
    };
    submit_payload(&handle, req.target, payload, origin, req.required_blob).await
}

/// An operator-authorized arbitrary module op, authored as the node. User
/// clients submit signed frames instead; this endpoint never claims a user.
async fn submit_raw(
    State(handle): State<NodeHandle>,
    axum::extract::Path(target): axum::extract::Path<String>,
    axum::extract::Query(prerequisite): axum::extract::Query<SubmitBlob>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    let bounded_target = !target.is_empty() && target.len() <= node::MAX_TARGET_BYTES;
    if !bounded_target { return error_response(StatusCode::BAD_REQUEST, "invalid module target"); }
    let body = match body {
        Ok(body) => body,
        Err(error) => return error_response(error.status(), &error.body_text()),
    };
    submit_payload(
        &handle,
        target,
        body.to_vec(),
        signed_req::acting_origin(None),
        prerequisite.required_blob,
    )
    .await
}

async fn submit_payload(
    handle: &NodeHandle,
    target: String,
    payload: Vec<u8>,
    origin: Vec<u8>,
    required_blob: Option<[u8; 32]>,
) -> Response {
    if let Err(reason) = require_local_blob(handle, required_blob.as_ref()) {
        return error_response(StatusCode::BAD_REQUEST, reason);
    }
    let (reply, rx) = oneshot::channel();
    if let Err(resp) = handle
        .send(NodeCommand::Submit {
            target,
            payload: payload.clone(),
            required_blob,
            origin,
            reply,
        })
        .await
    {
        return resp;
    }
    match rx.await {
        Ok(Ok(block)) => {
            // stage the op's bytes only AFTER the commit so a rejected op leaves
            // nothing behind. put_chunk keys by sha256, so the digest IS the
            // op's content address (fetchable via /v1/files/blob/{op_hash}).
            let op_hash = hex_bytes(&handle.blobs.put_chunk(payload));
            Json(SubmitReceipt {
                height: block.height,
                root_hash: block.root_hash,
                op_hash,
            })
            .into_response()
        }
        Ok(Err(err)) => error_response(StatusCode::BAD_REQUEST, &err),
        Err(_) => actor_gone(),
    }
}

fn require_local_blob(handle: &NodeHandle, digest: Option<&[u8; 32]>) -> Result<(), &'static str> {
    let Some(digest) = digest else { return Ok(()) };
    let available = handle.blobs.has_verified_chunk(digest);
    if !available {
        return Err("required blob is not available on this node");
    }
    Ok(())
}

/// POST /v1/submit/frame — an ALREADY-SIGNED op frame (`application/octet-stream`,
/// the exact bytes [`node::encode_frame`] produced), answered with the same
/// [`SubmitReceipt`] `/v1/submit` returns.
///
/// this lane exists because `/v1/submit`'s `origin` is a caller-supplied STRING:
/// the embedded daemon honours it, `bin/node` throws it away and signs with its
/// own node key, so nothing submitted there can carry authorship consensus is
/// able to check. a frame can — its origin IS its verified signer, bound to
/// `(seq, target, payload, required_blob)` under `FRAME_NS`, which every honest validator
/// re-verifies identically. that is what lets an agent's ephemeral session key
/// act for itself instead of borrowing the node's identity.
///
/// the signature is verified HERE, before the frame reaches any actor: a frame
/// that does not parse, or whose signature does not bind, is a 400 carrying the
/// codec's verbatim reason and never enters a store or an orderer. the actors
/// verify again where it matters (the validator's `submit_frame` re-checks
/// before it pins) — this is the cheap gate, not the only one.
async fn submit_frame(
    State(handle): State<NodeHandle>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let frame = match body {
        Ok(bytes) => bytes,
        Err(rejection) => return error_response(rejection.status(), &rejection.body_text()),
    };
    let (payload, required_blob) = match node::decode_frame_with_blob(&frame) {
        // the origin is DELIBERATELY dropped here: the http layer never tells an
        // actor who signed — the actor re-derives that from the bytes (or, on
        // the validator, `submit_frame` does). one authority on authorship.
        Ok((_origin, msg, required_blob)) => (msg.payload, required_blob),
        Err(err) => return error_response(StatusCode::BAD_REQUEST, &err.to_string()),
    };
    if let Err(reason) = require_local_blob(&handle, required_blob.as_ref()) {
        return error_response(StatusCode::BAD_REQUEST, reason);
    }
    let (reply, rx) = oneshot::channel();
    if let Err(resp) = handle
        .send(NodeCommand::SubmitFrame {
            frame: frame.to_vec(),
            reply,
        })
        .await
    {
        return resp;
    }
    match rx.await {
        Ok(Ok(block)) => {
            // the receipt's op_hash addresses the op PAYLOAD — the module's own
            // bytes, exactly as the frameless lane stages them. the frame
            // envelope is transport (signature, seq), not content.
            let op_hash = hex_bytes(&handle.blobs.put_chunk(payload));
            Json(SubmitReceipt {
                height: block.height,
                root_hash: block.root_hash,
                op_hash,
            })
            .into_response()
        }
        Ok(Err(err)) => error_response(StatusCode::BAD_REQUEST, &err),
        Err(_) => actor_gone(),
    }
}

async fn query(State(handle): State<NodeHandle>, Json(req): Json<QueryRequest>) -> Response {
    let req_bytes = serde_json::to_vec(&req.query).expect("a decoded json value re-serializes");
    let (reply, rx) = oneshot::channel();
    if let Err(resp) = handle
        .send(NodeCommand::Query {
            target: req.target,
            req: req_bytes,
            reply,
        })
        .await
    {
        return resp;
    }
    match rx.await {
        Ok(Ok(bytes)) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => Json(value).into_response(),
            Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "module reply was not json",
            ),
        },
        Ok(Err(err)) => error_response(StatusCode::BAD_REQUEST, &err),
        Err(_) => actor_gone(),
    }
}

/// POST /v1/query/reader — the AUTHENTICATED read lane.
///
/// Same `{target, query}` body as `/v1/query`, and the same committed state
/// behind it. The one difference is the whole point: the caller proves
/// possession of a key with the data-plane signature trio
/// ([`crate::signed_req`]), and that verified key reaches the module as
/// [`host::Origin::External`] — the SAME `Env::origin` field a write's
/// authority arrives in, so a module gates a protected read with the vocabulary
/// it already gates writes with.
///
/// ## why the reader cannot ride in the body
///
/// It was tried. A `{"reader": …}` envelope inside `query` is forgeable by
/// anyone who can POST the UNAUTHENTICATED `/v1/query`, which is open to
/// anything that can dial this port — a sandboxed guest reaching this listener
/// through its vsock tunnel included (see [`crate::signed_req`]'s module doc).
/// A caller supplies request BYTES and nothing else; putting the reader outside
/// those bytes is what makes it unforgeable, and no refusal rule on the open
/// lane is needed to keep it that way.
///
/// ## fail-closed, and where
///
/// This route establishes WHO. It deliberately does not decide what that reader
/// may see: only the module holds the roster. The corollary is the module's
/// obligation — a module serving protected content MUST refuse
/// [`host::Origin::System`] for it, because System is exactly what the open
/// lane still passes. A module that forgets is open on `/v1/query`, and no
/// route can fix that for it.
///
/// This authenticates the READER at the RPC edge. It is not confidentiality:
/// bodies live in replicated committed state, so every validator can read them.
/// Nothing here promises end-to-end encryption.
async fn query_as_reader(
    State(handle): State<NodeHandle>,
    OriginalUri(uri): OriginalUri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let path_and_query = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    // the signature covers the EXACT bytes below, so verify before decoding:
    // an unauthenticated caller learns nothing about the body's shape.
    let reader = match crate::signed_req::verify_signed_request(
        &handle,
        &axum::http::Method::POST,
        path_and_query,
        &headers,
        &body,
    ) {
        Ok(key) => key,
        Err(refusal) => return crate::signed_req::refuse(path_and_query, refusal),
    };
    let Ok(req) = serde_json::from_slice::<QueryRequest>(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "body must be {target, query}");
    };
    let req_bytes = serde_json::to_vec(&req.query).expect("a decoded json value re-serializes");
    let (reply, rx) = oneshot::channel();
    if let Err(resp) = handle
        .send(NodeCommand::QueryAs {
            target: req.target,
            req: req_bytes,
            reader,
            reply,
        })
        .await
    {
        return resp;
    }
    match rx.await {
        Ok(Ok(bytes)) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => Json(value).into_response(),
            Err(_) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "module reply was not json",
            ),
        },
        Ok(Err(err)) => error_response(StatusCode::BAD_REQUEST, &err),
        Err(_) => actor_gone(),
    }
}

/// GET /v1/status — the last boundary snapshot the owning actor published,
/// with live operations overlaid, straight off the handle's [`StatusCell`].
/// deliberately NEVER crosses the command lane: a sync/catch-up stage keeps
/// the pump busy for whole stages, and status must answer through that.
async fn status(State(handle): State<NodeHandle>) -> Response {
    Json(handle.status_cell().current()).into_response()
}

/// GET /v1/peers — the direct-peer sample (see [`peers::PeersView`]): who the
/// mesh holds open right now, cumulative per-peer traffic counters, and each
/// peer's statesync progression where one exists. composed OFF the command
/// lane like status: the connection/traffic counters parse from the live
/// exposition source, the committed facts (roles, height, epoch) come from
/// the standing the owning actor last published.
async fn peers(State(handle): State<NodeHandle>) -> Response {
    let cell = handle.status_cell();
    let Some(exposition) = cell.exposition() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no metrics exposition is wired on this daemon",
        );
    };
    let standing = cell.peers_standing();
    let view = peers::peers_from_exposition(
        &exposition,
        stream::unix_millis(),
        standing.height,
        standing.epoch,
    )
    .with_roles(&standing.validators, &standing.residents)
    .with_builds(&standing.builds);
    Json(view).into_response()
}

/// POST /v1/admin/shutdown — ask the process to exit gracefully. lives on the
/// owner-gated admin namespace: it was on the unauthenticated public
/// surface, reachable by anything that could dial the port.
pub(crate) async fn shutdown(State(handle): State<NodeHandle>) -> Response {
    // reply first, then signal — the connection closes before the process does.
    handle.request_shutdown();
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// POST /v1/log-filter — retune the log level of a RUNNING node.
///
/// ```text
/// ducktape node log-filter 'info,ducktape::join=debug'
/// ```
///
/// RUST_LOG is read once at boot, so without this route every `debug!` in the
/// tree is unreachable without a restart — and restarting a wedged node destroys
/// the state you restarted it to look at.
///
/// AUTH: it MUTATES the running PROCESS and reads no acting identity, so it is
/// one of the node-level routes (`signed_req`, `Authority::Operator`): this
/// node's operator credential, or a signature by the key it knows as its
/// operator's. turning `ducktape::x=trace` on is a real denial-of-service
/// against the node's own disk — `daemon.log` has no rate limit — and any key a
/// caller minted for itself must not buy it. `ducktape node log-filter` signs
/// with the active wallet key, which is the key the node read at boot.
async fn log_filter(body: String) -> Response {
    match crate::log::set_filter(body.trim()) {
        Ok(()) => (StatusCode::OK, body).into_response(),
        Err(err) => error_response(StatusCode::BAD_REQUEST, &err),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HuddleNodeProofBody {
    channel_id: String,
}

#[derive(Serialize)]
struct HuddleNodeProofResponse {
    node: String,
    node_proof: String,
}

/// POST /v1/huddle/node-proof `{"channel_id": "…"}` — mints the ed25519
/// signature `chat::ChatMsg::JoinHuddle.node_proof` needs: THIS node's own
/// identity key, signing `chat::huddle_join_preimage(channel_id, signer)`
/// under `chat::HUDDLE_JOIN_NS`. 503 on a daemon with no mesh identity (the
/// embedded local daemon, `bin/noded`) — huddle routing has nothing to name
/// there either.
///
/// AUTH: a SIGNED request (`signed_req`, `Lane::HuddleProof`) by a key that
/// holds an identity account. the proof binds the VERIFIED signer, never a
/// caller-supplied user, so a roster entry can only ever carry a node that
/// agreed to route that exact person — and the node agrees only for a member
/// of its own network (`key_without_account` otherwise). the operator
/// credential, which the guard admits on every lane, names no person to bind
/// and is refused here: a device that hosts this node signs like any other.
async fn huddle_node_proof(
    State(handle): State<NodeHandle>,
    signed: Option<axum::Extension<SignedBy>>,
    Json(body): Json<HuddleNodeProofBody>,
) -> Response {
    let Some(signer) = handle.node_signer.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "this node has no mesh identity to prove a huddle node key with",
        );
    };
    let Some(axum::Extension(SignedBy(user))) = signed else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "a huddle node proof binds the key that signed the request; sign it as the joining user",
        );
    };
    if let Err(refused) = call::account_holder(&handle, user.clone()).await {
        return refused;
    }
    let preimage = chat::huddle_join_preimage(&body.channel_id, &user);
    let node_proof = signer.sign(chat::HUDDLE_JOIN_NS, &preimage);
    Json(HuddleNodeProofResponse {
        node: hex_bytes(signer.public_key().as_ref()),
        node_proof: hex_bytes(node_proof.as_ref()),
    })
    .into_response()
}

/// POST /v1/invite `{"ttl_days": N}` — mint one bearer invite and answer
/// `{"invite": "🦆…"}`. `ttl_days` defaults to and is bounded by the ONE
/// policy every door shares (`workspace_config::{DEFAULT_INVITE_TTL_DAYS,
/// INVITE_TTL_DAYS}`), so this route and `ducktape node invite` mint the same
/// invite for the same request.
///
/// The RUNNING node mints it because minting is a write to the node's OWN
/// files: it folds this member's dial hint into the network descriptor and
/// saves it, and it reads the persisted mesh state for the member fronts a
/// joiner brings its tunnel up against. Doing that from a second process races
/// the daemon over both.
///
/// 503 when the embedder wired no minter — a daemon with no workspace has no
/// descriptor to fold a hint into.
///
/// AUTH: a bearer invite is a real capability — a right to join this mesh for
/// up to 365 days — and this handler reads no acting identity, so possession of
/// a self-minted key buys nothing here. It is node-level
/// (`signed_req`, `Authority::Operator`): this node's operator credential from
/// a local process that can read its workspace, or a signature by the key the
/// node knows as its operator's. The desktop app presents the former (it
/// already reads the same workspace for the service-link token).
async fn mint_invite(
    State(handle): State<NodeHandle>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let requested = body
        .as_ref()
        .and_then(|Json(value)| value["ttl_days"].as_u64())
        .unwrap_or(DEFAULT_INVITE_TTL_DAYS);
    if !INVITE_TTL_DAYS.contains(&requested) {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "ttl_days must be between {} and {}",
                INVITE_TTL_DAYS.start(),
                INVITE_TTL_DAYS.end()
            ),
        );
    }
    // the mint reads and rewrites files; it does not belong on a runtime worker.
    let cell = handle.status_cell();
    let minting = tokio::task::spawn_blocking(move || cell.mint_invite(requested)).await;
    let Ok(minted) = minting else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the invite mint panicked",
        );
    };
    let Some(minted) = minted else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no invite minter is wired on this daemon",
        );
    };
    match minted {
        Ok(invite) => (
            StatusCode::OK,
            Json(serde_json::json!({ "invite": invite })),
        )
            .into_response(),
        Err(why) => error_response(StatusCode::BAD_REQUEST, &why),
    }
}

/// body cap for the op-receipt blob lane. a receipt-lane bound only —
/// unrelated to duckfs chunking, which rides the op stream.
async fn limit_blob_uploads(
    State(slots): State<std::sync::Arc<tokio::sync::Semaphore>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let uploads_blob = request.method() == axum::http::Method::POST
        && request.uri().path() == "/v1/files/blob";
    if !uploads_blob { return next.run(request).await; }
    let Ok(_permit) = slots.try_acquire() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "blob upload capacity exhausted");
    };
    next.run(request).await
}

/// POST /v1/files/blob — raw receipt bytes in, `{"digest":"<64-hex>"}` out.
///
/// bytes go straight into the node-local blob store; NOTHING reaches the node
/// actor and no op is submitted.
///
/// THE BODY HAS NO SIZE LIMIT, and streams: it is written to the store's
/// staging directory frame by frame and named by its own hash once it ends, so
/// what a caller uploads costs this process one frame of memory and the disk
/// the bytes occupy. A git push of a whole repository's history arrives here,
/// and a limit on it would be a limit on what anyone may push.
async fn put_blob(
    State(handle): State<NodeHandle>,
    proof: Option<axum::Extension<signed_req::DeferredProof>>,
    body: axum::body::Body,
) -> Response {
    use futures::StreamExt as _;
    let mut ingest = match handle.blobs.ingest() {
        Ok(ingest) => ingest,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let mut frames = body.into_data_stream();
    while let Some(frame) = frames.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            // the upload died in flight; the ingest's Drop takes its file
            // with it.
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("body: {e}")),
        };
        if let Err(e) = ingest.append(&frame) {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
        }
    }
    // name the bytes, THEN admit them: an unsealed ingest is nameless and an
    // unadmitted one is never published, so a signature that does not bind
    // these exact bytes leaves nothing behind.
    let digest = match ingest.seal() {
        Ok(digest) => digest,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    // no proof extension means the gate admitted the request itself (this
    // node's operator credential); one means the gate deferred to here.
    if let Some(axum::Extension(proof)) = proof
        && let Err(refusal) = proof.verify(&handle, &digest)
    {
        return error_response(refusal.status(), refusal.message());
    }
    match ingest.publish() {
        Ok(digest) => Json(serde_json::json!({ "digest": hex_bytes(&digest) })).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /v1/files/blob/{digest} — chunk bytes back out of the node-local store.
/// a malformed digest (anything but 64 lowercase hex chars) is a 400; a
/// well-formed digest this node holds no bytes for is a 404.
async fn get_blob(State(handle): State<NodeHandle>, Path(digest): Path<String>) -> Response {
    let Some(raw) = duckfs_core::from_hex_32(&digest) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "digest must be 64 characters of lowercase hex",
        );
    };
    match handle.blobs.get_chunk(&raw) {
        Some(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        None => error_response(StatusCode::NOT_FOUND, "no chunk with that digest"),
    }
}

/// serve the client surface on `listener` until a shutdown request lands
/// (POST /v1/admin/shutdown). the caller owns the runtime this runs on; the host
/// actor lives elsewhere and is reachable only through `handle`'s command
/// lane — which is what lets ANY binary that owns a host (the embedded daemon,
/// the p2p validator) stand up the identical surface.
pub async fn serve(listener: tokio::net::TcpListener, handle: NodeHandle) -> std::io::Result<()> {
    let shutdown = handle.clone();
    // connect-info is threaded so the admin namespace's guard can read the peer
    // address (the `Loopback` exposure refuses non-loopback peers). every other
    // route ignores it.
    axum::serve(
        listener,
        router(handle).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
    .await
}

/// what a ws upgrade may ask for before it has a socket.
#[derive(serde::Deserialize)]
struct WsParams {
    /// one dispatch id, to be admitted as its creator
    /// ([`stream::admit_run_reader`]). Absent on every other client, which is
    /// why this surface stays open in `signed_req::required_authority`: the
    /// proof is demanded by the ASK, not by the route.
    run: Option<String>,
}

async fn ws(
    State(handle): State<NodeHandle>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(params): Query<WsParams>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // A RUN ASK IS PROVED BEFORE THE UPGRADE. An unadmitted caller gets an HTTP
    // refusal and no socket — it never reaches a state where a subscribe could
    // be tried, and it learns nothing about whether the run exists.
    let reader_of = match params.run {
        None => None,
        Some(run) => {
            let path_and_query = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
            if let Err(refused) =
                stream::admit_run_reader(&handle, &run, &headers, path_and_query).await
            {
                return refused;
            }
            Some(run)
        }
    };
    // unauthenticated surface: cap the frame/message tungstenite otherwise
    // defaults to 64 MiB, so a single frame cannot force a large buffer before
    // any handler gets to look at it (see `stream::MAX_WS_MESSAGE_BYTES`).
    upgrade
        .max_message_size(stream::MAX_WS_MESSAGE_BYTES)
        .max_frame_size(stream::MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| stream::stream_session(socket, handle, reader_of))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_wakes_every_surface_and_remains_sticky() {
        let (handle, _commands, _hub) = NodeHandle::channel();
        let first = handle.clone();
        let second = handle.clone();
        let waiters = async move {
            tokio::join!(first.shutdown_requested(), second.shutdown_requested());
        };
        let trigger = async {
            tokio::task::yield_now().await;
            handle.request_shutdown();
        };

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(waiters, trigger);
        })
        .await
        .expect("every registered surface wakes");
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle.shutdown_requested(),
        )
        .await
        .expect("shutdown remains visible to a late surface");
    }

    /// the explorer row round-trips through its stored index encoding — the
    /// one seam both binaries write and `GET /v1/blocks` reads.
    #[test]
    fn block_row_round_trips() {
        let record = BlockRecord {
            height: 7,
            hash: String::new(),
            commit_hash: "aa".repeat(32),
            ops: vec![
                RootOp {
                    proposer: "bb".repeat(32),
                    disposition: BlockDisposition::Applied,
                    target: "tasks".into(),
                    operations: Vec::new(),
                    payload: "{}".into(),
                    op_hash: "ee".repeat(32),
                },
                RootOp {
                    proposer: "cc".repeat(32),
                    disposition: BlockDisposition::Rejected,
                    target: "chat".into(),
                    operations: Vec::new(),
                    payload: "{\"m\":1}".into(),
                    op_hash: "ff".repeat(32),
                },
            ],
        };
        let row = block_row(&record);
        let back: BlockRecord = serde_json::from_slice(&row).expect("row is json");
        assert_eq!(back.height, 7);
        assert_eq!(back.hash, "", "frameless lanes keep an honest empty hash");
        assert_eq!(back.ops.len(), 2, "the block aggregated two member ops");
        assert_eq!(back.ops[0].proposer, "bb".repeat(32));
        assert_eq!(back.ops[1].op_hash, "ff".repeat(32));
        // the wire keys are snake_case — clients read these fields verbatim.
        let json: serde_json::Value = serde_json::from_slice(&row).unwrap();
        assert!(json.get("commit_hash").is_some());
        assert!(json["ops"][0].get("op_hash").is_some());
    }

    #[test]
    fn payload_preview_caps_and_marks_truncation() {
        assert_eq!(payload_preview(b"{\"k\":\"v\"}"), "{\"k\":\"v\"}");
        let long = "x".repeat(PAYLOAD_PREVIEW_MAX + 10);
        let preview = payload_preview(long.as_bytes());
        assert_eq!(preview.chars().count(), PAYLOAD_PREVIEW_MAX + 1);
        assert!(preview.ends_with('…'), "truncation is visible");
        // invalid utf-8 renders lossily rather than erroring.
        assert_eq!(payload_preview(&[0xff, 0xfe]), "\u{fffd}\u{fffd}");
        // a binary payload (every byte invalid utf-8, so every byte becomes a
        // 3-byte replacement char) is decoded only through the bounded prefix
        // — the cap holds and the clip is visible. this is the drain-path
        // guard: a 1 MiB photo chunk must never be lossy-decoded whole.
        let binary = vec![0xffu8; 8 * 1024];
        let preview = payload_preview(&binary);
        assert_eq!(preview.chars().count(), PAYLOAD_PREVIEW_MAX + 1);
        assert!(
            preview.ends_with('…'),
            "byte-clipped previews carry the mark"
        );
        // multi-byte text still fills its full char budget: 4 bytes/char is
        // the decode ceiling, so 1024 chars always fit the prefix.
        let wide = "😀".repeat(PAYLOAD_PREVIEW_MAX + 10);
        let preview = payload_preview(wide.as_bytes());
        assert_eq!(preview.chars().count(), PAYLOAD_PREVIEW_MAX + 1);
    }

    #[test]
    fn module_categories_group_the_genesis_set() {
        use ModuleCategory::*;
        for id in ["chat", "tasks", "inbox", "pages"] {
            assert_eq!(ModuleCategory::of(id), Workspace, "{id}");
        }
        for id in ["forge", "agent"] {
            assert_eq!(ModuleCategory::of(id), Developer, "{id}");
        }
        assert_eq!(ModuleCategory::of("automations"), Automation);
        // infra + internals fall to the System bucket — including ids this
        // daemon never composes at all: `valset`/`governance` are in
        // `PRODUCTION` but not `SIM_BASE`, so only the full `node` binary
        // registers them, and `kv` is in NEITHER — it reaches a composer only
        // through simnode's opt-in `--with-valset` (`SIM_VALSET`). plus
        // anything unknown, so the view never breaks on a new module.
        for id in [
            "files",
            "saga",
            "identity",
            "gateway",
            "kv",
            "valset",
            "governance",
            "totally-unknown",
        ] {
            assert_eq!(ModuleCategory::of(id), System, "{id}");
        }
    }
}
