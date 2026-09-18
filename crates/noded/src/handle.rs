//! the node-actor command lane ([`NodeCommand`]) and the router's shared
//! state ([`NodeHandle`]): every http handler talks to whichever actor owns
//! the non-Send `host::Host` exclusively through this seam.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use futures::SinkExt as _;
use futures::channel::{mpsc, oneshot};

use crate::call::PresenceLane;
use crate::gateway_http::{BrowserGateway, GatewayLane};
use crate::gateway_ws_token::WsTokenStore;
use crate::metrics::NodeMetrics;
use crate::stream::{LogRing, StreamHub};
use crate::{BlockSummary, NodeStatus, OperationalStatus, error_response};

/// inbound command backlog before submit/query callers see backpressure.
pub(crate) const COMMAND_BUFFER: usize = 64;
/// internal block wakeups buffered per lagging websocket subscriber.
pub(crate) const EVENT_BUFFER: usize = 64;

/// why the actor refused, in the two pieces a caller actually needs: a stable
/// snake_case token to branch on, and the sentence whoever refused wrote.
///
/// they are separate because they are BOUNDED separately — a client that clips
/// a long message must never clip the token with it — and because a screen that
/// keys its behaviour off prose keys it off nothing. a module's refusal brings
/// its OWN token, so this carries a `String` rather than a literal; it stays
/// greppable and countable like every other `reason` in this tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub reason: String,
    pub message: String,
}

impl Refused {
    pub fn new(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            message: message.into(),
        }
    }

    /// the kernel's own refusal, split. ONE match with no `_` arm: a new
    /// [`sdk::Error`] variant fails this build until it is given a token and a
    /// sentence, which is why the split lives here rather than in a `Display`
    /// impl on the enum — `sdk` is the deterministic module ABI, compiled into
    /// every module guest, and what a screen says is not its business.
    ///
    /// the sentence deliberately drops the variant's NAME. `sdk::Error`'s
    /// `Display` is its `Debug`, so `to_string()` yields
    /// `Module(<reason>: <sentence>)` — an envelope that then reaches a person,
    /// and that every reader downstream has to peel back off.
    pub fn of(error: &sdk::Error) -> Self {
        let (reason, message): (String, String) = match error {
            sdk::Error::UnknownModule(id) => (
                "unknown_module".into(),
                format!("no module is registered as {id}"),
            ),
            sdk::Error::SelfQuery => (
                "self_query".into(),
                "a module reads its own state through itself, not through a query".to_owned(),
            ),
            sdk::Error::QueryUnsupported => (
                "query_unsupported".into(),
                "this module answers no queries".to_owned(),
            ),
            sdk::Error::SyncUnsupported => (
                "sync_unsupported".into(),
                "this module serves no state sync".to_owned(),
            ),
            sdk::Error::SwapUnsupported => (
                "swap_unsupported".into(),
                "this module's code is the node binary itself, so it cannot be swapped".to_owned(),
            ),
            sdk::Error::BudgetExceeded => (
                "budget_exceeded".into(),
                "the follow-up drain exceeded its dispatch budget".to_owned(),
            ),
            // the module's own words, whole — and its own TOKEN: nothing here
            // paraphrases a refusal it did not write, and nothing re-classifies
            // one it did.
            sdk::Error::Module { reason, sentence } => (reason.clone(), sentence.clone()),
        };
        Self { reason, message }
    }

    /// a refusal that reached this node as ONE framed string
    /// (`<reason>: <sentence>`) rather than as an [`sdk::Error`]: a drained
    /// frame's captured reason, or a custodian's relayed rejection.
    ///
    /// a string that named no class stays UNCLASSIFIED — a token is never
    /// invented for one here, because a made-up word is one every consumer
    /// would then have to tell apart from a word a module actually chose.
    pub fn framed(said: &str) -> Self {
        match sdk::refusal::decode(said) {
            Some((reason, sentence)) => Self::new(reason, sentence),
            None => Self::new("unframed_refusal", said),
        }
    }

    /// a write's refusal. a deterministic rejection is the module's own, whole;
    /// a boundary fault is THIS NODE's and says so — the two must not read
    /// alike, because one is the caller's to fix and the other is not.
    ///
    /// `SubmitError`'s `Display` writes `op rejected: ` in front of the
    /// sentence, which is the write lane's version of the same envelope.
    pub fn of_submit(error: &host::SubmitError) -> Self {
        match error {
            host::SubmitError::Rejected(rejected) => Self::of(rejected),
            host::SubmitError::Fatal(fault) => Self::new("boundary_fault", fault.to_string()),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// a request to the actor that owns the host. replies cross the channel as
/// wire-ready types so the http layer stays free of sdk conversions — the
/// refusal included, which is what [`Refused`] is for.
pub enum NodeCommand {
    Submit {
        target: String,
        payload: Vec<u8>,
        /// Opaque content that must be present before this operation is admitted.
        required_blob: Option<[u8; 32]>,
        /// `Origin::External` bytes for this block: the key a request's
        /// signature proved possession of on a gated route
        /// ([`crate::signed_req::SignedBy`]), or — on the frameless
        /// `/v1/submit` lane only — the caller's CLAIMED string
        /// (see [`crate::SubmitRequest::origin`], an open finding).
        origin: Vec<u8>,
        reply: oneshot::Sender<Result<BlockSummary, Refused>>,
    },
    /// take custody of an ALREADY-SIGNED op frame (`POST /v1/submit/frame`).
    /// carries the RAW frame bytes: the origin rides INSIDE them as the
    /// signature's verified signer, so no lane consults a caller string and no
    /// lane may re-sign — a validator that re-framed this with its own node key
    /// would destroy the exact authorship the lane exists to carry (an agent's
    /// session key). the bytes are verified before they reach any actor, and
    /// every actor verifies again where it must.
    SubmitFrame {
        frame: Vec<u8>,
        reply: oneshot::Sender<Result<BlockSummary, Refused>>,
    },
    /// read committed module state as the NODE ITSELF (`host::Origin::System`)
    /// — the widest reader there is. Right for the node's own reads and for the
    /// unauthenticated `/v1/query` lane, whose caller has proven nothing: a
    /// module that serves protected content refuses System for it.
    Query {
        target: String,
        req: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, Refused>>,
    },
    /// read committed module state as an AUTHENTICATED reader
    /// (`POST /v1/query/reader`). `reader` is the ed25519 key a request's
    /// data-plane signature proved possession of, and the actor hands it to the
    /// host as `Origin::External(reader)` — the same field a write's authority
    /// arrives in.
    ///
    /// It is a FIELD ON THIS COMMAND and not a field in `req` on purpose. A
    /// caller supplies request BYTES; it cannot reach this struct. Any scheme
    /// that carried the reader inside `req` would be forgeable by anyone who can
    /// POST the unauthenticated `/v1/query` — the whole reason this variant
    /// exists.
    QueryAs {
        target: String,
        req: Vec<u8>,
        /// the VERIFIED signer ([`crate::signed_req::verify_signed_request`]).
        /// Never a caller-supplied identifier.
        reader: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, Refused>>,
    },
}

/// the committed facts a `/v1/peers` sample needs from the actor: valset
/// standing (hex key sets), the served height, and the epoch. published
/// beside the status snapshot at the same boundaries; the peer/traffic
/// counters themselves are parsed LIVE from the wired exposition source.
#[derive(Clone, Default)]
pub struct PeersStanding {
    pub validators: std::collections::BTreeSet<String>,
    pub residents: std::collections::BTreeSet<String>,
    pub height: u64,
    pub epoch: Option<u64>,
    /// peer key hex -> the build stamp that peer reported about ITSELF, for
    /// the lanes that have heard one. empty on a lane that has heard none —
    /// the mesh gossips no stamp, so this is only ever what a lane learned
    /// from a peer it polled ([`crate::peers::PeerView::build`]).
    pub builds: std::collections::BTreeMap<String, String>,
}

/// the observability snapshot cell: the actor that owns the host PUBLISHES
/// its projections at each boundary it settles — the complete [`NodeStatus`]
/// and the peers standing — and the http handlers read the last ones
/// published without ever crossing the command lane. that read-side
/// independence is the point: a sync/catch-up stage keeps the pump away from
/// its command queue for whole stages, and the observability surface
/// (status, peers, /metrics) must keep answering through exactly that state.
#[derive(Clone, Default)]
pub struct StatusCell {
    inner: Arc<StatusCellInner>,
}

#[derive(Default)]
struct StatusCellInner {
    /// the last-published snapshot. publish swaps the WHOLE struct under one
    /// write, so a read reflects exactly one boundary — never a torn one.
    snapshot: std::sync::RwLock<NodeStatus>,
    /// the last-published peers standing (same whole-struct-swap contract).
    standing: std::sync::RwLock<PeersStanding>,
    /// the live operations source — the metrics' shared projection, wired
    /// once at boot by daemons that register [`NodeMetrics`]. a read overlays
    /// it so phase and sync progress stay live BETWEEN boundary publishes
    /// (they move mid-stage, exactly when no boundary publish can happen).
    /// unwired (simnode), the published operations serve as-is.
    operations: std::sync::OnceLock<Arc<std::sync::RwLock<OperationalStatus>>>,
    /// the chain id — a boot-time fact, wired once and overlaid on every read
    /// so no boundary publish has to remember it. Unwired = the published
    /// (empty) value serves as-is.
    chain_id: std::sync::OnceLock<String>,
    /// the live OpenMetrics exposition source — a registry encoder wired once
    /// at boot (the commonware context's `encode`). `/metrics`, `/v1/peers`,
    /// and the ws metrics topic all read it directly; the registry is shared
    /// state, so encoding it never needs the actor.
    exposition: std::sync::OnceLock<Arc<dyn Fn() -> String + Send + Sync>>,
    /// the invite minter — wired once at boot by an embedder that owns a
    /// workspace (its `node.toml`, descriptor, identity and WireGuard key).
    ///
    /// Minting an invite is the RUNNING node's business twice over: it folds
    /// this member's dial hint into the descriptor and SAVES it, and it reads
    /// the persisted mesh state for the fronts a joiner can bring its tunnel up
    /// against. A second process doing that races the daemon over its own
    /// files. Unwired (simnode, an embedder with no workspace), `/v1/invite`
    /// answers 503 — the same shape as an unwired exposition.
    invite_minter: std::sync::OnceLock<Arc<InviteMinter>>,
    /// the netstack backend swapper — wired once at boot by an embedder that
    /// owns a reachability plane. Unwired (simnode, the embedded daemon, a
    /// node with no `wireguard_listen`), `POST /v1/admin/netstack/swap`
    /// answers 503: there is no plane to swap.
    netstack_swapper: std::sync::OnceLock<Arc<NetstackSwapper>>,
}

/// Which machine the reachability plane is wanted on. The component path is
/// read on the NODE, not by the caller: the route takes a path on the node's
/// own disk, exactly like `module-code` staging.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetstackSwapRequest {
    /// `{"component": "<path>"}` — a `ducktape:netstack` component on disk.
    Component(PathBuf),
    /// component bytes already in hand — the governance reconciler's variant:
    /// the designated component is a verified chunk on the node's blob plane,
    /// so writing it to disk to read it straight back would be the only I/O
    /// in the path. NOT on the wire (`serde(skip)`): the admin route is a
    /// path route by design, and no caller ships bytes through it.
    #[serde(skip)]
    Bytes(Vec<u8>),
}

/// Swap the reachability plane's backend, answering the new backend's name or
/// the plane's refusal reason. A refusal leaves the running machine untouched —
/// that contract is the executor's, and nothing here retries.
pub type NetstackSwapper = dyn Fn(NetstackSwapRequest) -> futures::future::BoxFuture<'static, Result<String, String>>
    + Send
    + Sync;

/// Mint one bearer invite valid for `ttl_days`, answering the paste blob and
/// the notes the mint left on it.
pub type InviteMinter = dyn Fn(u64) -> Result<MintedInvite, String> + Send + Sync;

/// A minted invite as `POST /v1/invite` answers it: the paste blob, and every
/// thing the mint could not do, empty when it could do everything.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MintedInvite {
    pub invite: String,
    pub notes: Vec<InviteNote>,
}

/// One thing a mint could not do. Never a refusal — the blob still admits a
/// joiner — but it changes what the blob can do (fewer paths, or none off this
/// machine), so it rides beside the blob for whoever hands the blob on:
/// `reason` is the stable snake_case token, `sentence` what to do about it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct InviteNote {
    pub reason: String,
    pub sentence: String,
}

impl StatusCell {
    /// publish a complete snapshot — one whole-struct swap.
    pub fn publish(&self, status: NodeStatus) {
        *self
            .inner
            .snapshot
            .write()
            .expect("status snapshot lock poisoned") = status;
    }

    /// publish the boundary this node RECOVERED off local disk, before any
    /// consensus boundary exists to publish a whole snapshot from.
    ///
    /// A restarting node holds its whole chain on disk for the length of the
    /// recovery window, and the boot publish leaves `height` 0 and `root_hash`
    /// empty — an answer indistinguishable from a brand-new empty node, on the
    /// two numbers a human and every operator script read first. This is the
    /// same pair a boundary publish carries, written together under one lock,
    /// so a read still sees one boundary and never a torn one.
    pub fn publish_recovered(&self, height: u64, root_hash: String) {
        let mut snapshot = self
            .inner
            .snapshot
            .write()
            .expect("status snapshot lock poisoned");
        snapshot.height = height;
        snapshot.root_hash = root_hash;
    }

    /// wire the chain id this daemon serves — once, at boot; a second call is
    /// ignored (the first boot fact wins, like every other `OnceLock` here).
    pub fn wire_chain_id(&self, chain_id: String) {
        let _ = self.inner.chain_id.set(chain_id);
    }

    /// publish the peers standing — one whole-struct swap, same contract as
    /// the status snapshot.
    pub fn publish_peers(&self, standing: PeersStanding) {
        *self
            .inner
            .standing
            .write()
            .expect("peers standing lock poisoned") = standing;
    }

    /// the last-published peers standing (zeroed before the first publish —
    /// an empty sample with no roles, the honest pre-boundary answer).
    pub fn peers_standing(&self) -> PeersStanding {
        self.inner
            .standing
            .read()
            .expect("peers standing lock poisoned")
            .clone()
    }

    /// wire the live operations overlay to the metrics' shared projection.
    /// once per process; a second wiring is a programming error.
    pub fn wire_metrics(&self, metrics: &NodeMetrics) {
        self.inner
            .operations
            .set(metrics.operations_handle())
            .expect("status cell operations source wired twice");
    }

    /// wire the live OpenMetrics exposition source (the registry encoder).
    /// once per process; a second wiring is a programming error.
    pub fn wire_exposition(&self, encode: impl Fn() -> String + Send + Sync + 'static) {
        if self.inner.exposition.set(Arc::new(encode)).is_err() {
            panic!("status cell exposition source wired twice");
        }
    }

    /// one live exposition sample, or `None` when no source is wired (a
    /// handle whose embedder registers no metrics — the routes answer 503).
    pub fn exposition(&self) -> Option<String> {
        self.inner.exposition.get().map(|encode| encode())
    }

    /// wire the invite minter. once per process; a second wiring is a
    /// programming error.
    pub fn wire_invite_minter(
        &self,
        mint: impl Fn(u64) -> Result<MintedInvite, String> + Send + Sync + 'static,
    ) {
        if self.inner.invite_minter.set(Arc::new(mint)).is_err() {
            panic!("status cell invite minter wired twice");
        }
    }

    /// One freshly minted invite, or `None` when no minter is wired (a node
    /// still starting, or an embedder with no workspace to mint from — the
    /// route answers 503).
    ///
    /// Blocking: the mint reads and REWRITES the descriptor and reads the
    /// persisted mesh state, so a caller on an async runtime owes this a
    /// blocking thread.
    pub fn mint_invite(&self, ttl_days: u64) -> Option<Result<MintedInvite, String>> {
        self.inner.invite_minter.get().map(|mint| mint(ttl_days))
    }

    /// wire the netstack backend swapper. once per process; a second wiring is
    /// a programming error.
    pub fn wire_netstack_swapper(
        &self,
        swap: impl Fn(
            NetstackSwapRequest,
        ) -> futures::future::BoxFuture<'static, Result<String, String>>
        + Send
        + Sync
        + 'static,
    ) {
        if self.inner.netstack_swapper.set(Arc::new(swap)).is_err() {
            panic!("status cell netstack swapper wired twice");
        }
    }

    /// Swap the plane's backend: the new backend's name, the plane's refusal,
    /// or `None` when no swapper is wired (no reachability plane on this node
    /// — the route answers 503).
    pub async fn swap_netstack(
        &self,
        request: NetstackSwapRequest,
    ) -> Option<Result<String, String>> {
        let swap = Arc::clone(self.inner.netstack_swapper.get()?);
        Some(swap(request).await)
    }

    /// the current status: the last-published boundary facts, with live
    /// operations overlaid when a metrics source is wired.
    pub fn current(&self) -> NodeStatus {
        let mut status = self
            .inner
            .snapshot
            .read()
            .expect("status snapshot lock poisoned")
            .clone();
        if let Some(chain_id) = self.inner.chain_id.get() {
            status.chain_id = chain_id.clone();
        }
        if let Some(operations) = self.inner.operations.get() {
            status.operations = operations.read().expect("operations lock poisoned").clone();
        }
        status
    }
}

/// the router's shared state: a command lane into the node actor, the
/// stream hub for websocket subscribers, the shutdown signal, and the
/// node-local blob store the files module shares.
#[derive(Clone)]
pub struct NodeHandle {
    pub(crate) cmds: mpsc::Sender<NodeCommand>,
    /// the `/v1/status` snapshot the owning actor publishes into; the status
    /// route reads it directly (the one read that never crosses `cmds`).
    pub(crate) status: StatusCell,
    pub(crate) hub: StreamHub,
    pub(crate) shutdown: tokio::sync::watch::Sender<bool>,
    /// the files blob lane. NOT a command into the actor: chunk bytes stay
    /// node-local by design (never consensus state, never an op), so the http
    /// handlers read/write this store directly.
    pub(crate) blobs: crate::blobs::BlobHandle,
    /// the forge module's on-disk repo base dir (`<storage>/<forge subdir>`);
    /// each named repo lives at `<forge_repo>/<name>` as a real libgit2 repo.
    /// threaded in so the git upload-pack (clone/fetch) handler can open a repo
    /// READ-ONLY and serve its objects — the ONE route that reads forge's git
    /// substrate directly instead of over the actor lane. `None` on a handle
    /// that never serves the git lane (the router tests' fake actor), which
    /// makes upload-pack a clean 500 there rather than a panic.
    pub(crate) forge_repo: Option<PathBuf>,
    /// the per-module derived index (fluent31-backed read models). node-local
    /// like `blobs`: the actor is the one WRITER as blocks commit;
    /// the `/v1/index/*` handlers read it directly through MVCC snapshots, so
    /// an index scan never crosses the actor command lane. `None` on a handle
    /// whose embedder configured no index (the router tests' fake actor) —
    /// index routes answer 503 there.
    pub(crate) index: Option<Arc<indexer::IndexStore>>,
    /// Pages presence request lane; absent when no overlay runtime exists.
    pub(crate) presence: Option<PresenceLane>,
    /// Purpose-specific gateway request lane. No raw peer, filesystem, or
    /// arbitrary socket proxy is exposed through the client surface.
    pub(crate) gateway: Option<GatewayLane>,
    /// Dedicated least-privilege browser origin for gateway rendering. It is
    /// a separate loopback listener, never the node API origin.
    pub(crate) browser_gateway: Option<BrowserGateway>,
    pub(crate) application_doors: Arc<crate::gateway_http::WsDoorLimit>,
    /// the root dir the duckfs workspace RPC materializes managed checkouts
    /// under (`<storage>/duckfs-workspaces`). node-local disk state, threaded in
    /// like `forge_repo`; `None` on a handle that never serves the seam (the
    /// router tests' fake handle), which makes `/v1/fs/workspaces` a clean 503.
    pub(crate) duckfs_workspaces: Option<PathBuf>,
    /// the node's code-plane stage lane (module-code fan-out). `None` on a
    /// daemon without a mesh — the admin stage route answers 503 there.
    pub(crate) code_stage: Option<crate::module_code::CodeStageLane>,
    /// the owner-gated control namespace's exposure + ownership config.
    /// the default (`Loopback`, no node key, NO operator token) FAILS
    /// CLOSED — it refuses every admin request; a real serve path mints a
    /// credential and passes it through [`Self::with_admin`].
    pub(crate) admin: crate::admin::AdminConfig,
    /// the node ↔ agent-daemon link. `None` on a handle that never wires one
    /// (router tests, an embedder that omits it) — a `ServiceAttach` is refused
    /// there and every workspace-gated ws topic fails closed. off-chain,
    /// node-local: never consensus state.
    pub(crate) service_link: Option<crate::service_link::ServiceLink>,
    /// the volatile catalog of service daemons signaling presence to this node.
    /// Always present (Default) — it is a bounded in-memory map, never durable
    /// and never consensus state, so there is no shape of node that wants the
    /// routes to 503. An entry confers no standing: `ducktape service enable`
    /// is the consent boundary.
    pub(crate) services: crate::services::ServiceCatalog,
    /// how many `POST /v1/index/{module}/view` calls (each a wasm query) or
    /// `GET /v1/index/status` calls (each a fold-trigger queue scan) may run
    /// concurrently off an axum worker — see
    /// [`crate::index::MAX_CONCURRENT_INDEX_VIEWS`]. one shared pool, not one
    /// per route: both are unauthenticated reads that can burn a worker
    /// thread's worth of CPU, so they compete for the same budget. `Arc` so
    /// every clone of this handle (one per accepted connection) shares the
    /// same gate; always present, since each route already 503s a handle
    /// with no index store wired.
    pub(crate) index_view_gate: Arc<tokio::sync::Semaphore>,
    /// this node's own mesh-identity signer — the SAME key `NodeStatus.public_key`
    /// publishes. wired directly (not through the `cmds` actor lane) so
    /// `POST /v1/huddle/node-proof` answers synchronously, like `status` does.
    /// `None` on a daemon with no mesh identity (the embedded local daemon,
    /// router tests) — that route 503s there.
    pub(crate) node_signer: Option<commonware_cryptography::ed25519::PrivateKey>,
}

impl NodeHandle {
    /// build the handle plus the actor-side ends: the command receiver the
    /// actor drains and the stream hub it publishes finalized blocks on.
    /// the blob store is born here — BEFORE genesis — so the embedding daemon
    /// can hand [`Self::blob_handle`] clones to forge and its block loop.
    pub fn channel() -> (Self, mpsc::Receiver<NodeCommand>, StreamHub) {
        Self::channel_with_log_ring(LogRing::default())
    }

    /// same as [`Self::channel`], but uses a caller-created log ring so a
    /// tracing layer can feed the same ring before the handle is fully wired.
    pub fn channel_with_log_ring(logs: LogRing) -> (Self, mpsc::Receiver<NodeCommand>, StreamHub) {
        let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_BUFFER);
        let hub = StreamHub::with_log_ring(EVENT_BUFFER, logs);
        let handle = Self {
            cmds: cmd_tx,
            status: StatusCell::default(),
            hub: hub.clone(),
            shutdown: tokio::sync::watch::channel(false).0,
            blobs: crate::blobs::BlobHandle::default(),
            forge_repo: None,
            index: None,
            presence: None,
            gateway: None,
            browser_gateway: None,
            application_doors: Arc::default(),
            duckfs_workspaces: None,
            code_stage: None,
            admin: crate::admin::AdminConfig::default(),
            service_link: None,
            services: crate::services::ServiceCatalog::default(),
            index_view_gate: Arc::new(tokio::sync::Semaphore::new(
                crate::index::MAX_CONCURRENT_INDEX_VIEWS,
            )),
            node_signer: None,
        };
        (handle, cmd_rx, hub)
    }

    /// swap the blob store for a persistent one rooted at `root` (write-
    /// through to `<root>/<sha256-hex>`, disk fallback on a memory miss) so
    /// node-local blobs — an agent's registered prompt above all — survive a
    /// daemon restart. still never consensus state, never in any root. must
    /// run BEFORE any [`Self::blob_handle`] clone is handed out (the daemons
    /// chain it right after [`Self::channel`]); an unusable root is a loud
    /// startup error, not a silently-forgetful store.
    pub fn with_blob_root(mut self, root: impl Into<PathBuf>) -> std::io::Result<Self> {
        self.blobs = crate::blobs::BlobHandle::persistent(root)?;
        Ok(self)
    }

    /// point this handle at the forge module's on-disk repo base dir so the git
    /// upload-pack (clone/fetch) handler can open `<forge_repo>/<name>` and serve
    /// its objects. the daemon passes the SAME base it hands `Forge::with_blobs`,
    /// so the http fetch lane reads exactly the repos consensus materializes.
    pub fn with_forge_repo(mut self, base: impl Into<PathBuf>) -> Self {
        self.forge_repo = Some(base.into());
        self
    }

    /// point this handle at the per-module derived index so the `/v1/index/*`
    /// routes can serve snapshot reads. the daemon passes the SAME store its
    /// actor feeds block-by-block.
    pub fn with_index_store(mut self, index: Arc<indexer::IndexStore>) -> Self {
        self.index = Some(index);
        self
    }

    /// Connect the Pages presence overlay request lane.
    pub fn with_presence(mut self, presence: PresenceLane) -> Self {
        self.presence = Some(presence);
        self
    }

    /// point this handle at the node's code-plane stage lane so the
    /// module-code admin route can fan staged artifacts out to members.
    /// only the p2p validator wires one — it owns the overlay the plane rides.
    pub fn with_code_stage(mut self, lane: crate::module_code::CodeStageLane) -> Self {
        self.code_stage = Some(lane);
        self
    }

    /// wire this node's own mesh-identity signer so `POST /v1/huddle/node-proof`
    /// can mint a `JoinHuddle.node_proof` for it — the SAME key `node_key` in
    /// [`Self::with_admin`] names. only a daemon with a real mesh identity
    /// wires one; a handle without it 503s the route.
    pub fn with_node_signer(
        mut self,
        signer: commonware_cryptography::ed25519::PrivateKey,
    ) -> Self {
        self.node_signer = Some(signer);
        self
    }

    /// configure the owner-gated control namespace. the full node
    /// passes its own consensus key (the salt every owner PoP is bound to),
    /// the operator's wallet key (whose account owns the plane) and the
    /// exposure the operator chose; a handle that leaves this at the default
    /// has no minted credential and so refuses all of admin.
    pub fn with_admin(mut self, admin: crate::admin::AdminConfig) -> Self {
        self.admin = admin;
        self
    }

    /// wire the node ↔ agent-daemon link so a `ServiceAttach` can take it and
    /// the workspace-gated ws topics have a secret to check. only the daemon
    /// wires one; a handle without it refuses both.
    pub fn with_service_link(mut self, link: crate::service_link::ServiceLink) -> Self {
        self.service_link = Some(link);
        self
    }

    /// the agent-daemon link, if one is wired.
    pub(crate) fn service_link(&self) -> Option<&crate::service_link::ServiceLink> {
        self.service_link.as_ref()
    }

    /// Has this caller proved it can read the node's OWN workspace?
    ///
    /// The one such proof the node has: the 0600 `service-link.token` a real
    /// serve path mints beside `node.toml`. The ws surface has no per-caller
    /// identity beyond it — `origin_guard` passes every `Origin`-less caller and
    /// a signaling hello confers nothing — so this is what a topic gate
    /// (`crate::stream::Admission::Workspace`) stands on. Reusing it invents no
    /// second scheme and adds no second secret file: a holder already owns the
    /// whole daemon link via `ServiceAttach`.
    ///
    /// Constant-time. `false` on a node that minted none (no workspace, or no
    /// daemon link wired) — fails closed.
    pub(crate) fn workspace_secret_matches(&self, presented: &str) -> bool {
        self.service_link
            .as_ref()
            .is_some_and(|link| link.link_token_matches(presented))
    }

    /// the volatile service signaling catalog (always present).
    pub fn services(&self) -> &crate::services::ServiceCatalog {
        &self.services
    }

    /// Point gateway requests at the full node's authenticated overlay
    /// stream. `net.duck` remains a local network-content read.
    pub fn with_gateway(mut self, lane: GatewayLane) -> Self {
        self.gateway = Some(lane);
        self
    }

    /// Enable gateway browsing on a separately bound loopback listener. The
    /// caller binds first so port 0 becomes an actual reportable port.
    pub fn with_browser_gateway(mut self, listen: SocketAddr) -> Self {
        self.browser_gateway = Some(BrowserGateway {
            listen,
            ws_tokens: Arc::new(WsTokenStore::new()),
            ws_doors: Arc::default(),
        });
        self
    }

    /// point this handle at the root dir the duckfs workspace RPC manages
    /// checkouts under. the daemon passes `<storage>/duckfs-workspaces`; an
    /// unset root makes `/v1/fs/workspaces` answer 503.
    pub fn with_duckfs_workspaces(mut self, root: impl Into<PathBuf>) -> Self {
        self.duckfs_workspaces = Some(root.into());
        self
    }

    /// the blob store this surface serves. the daemon hands clones to forge
    /// (push packfiles) and its block loop (op receipts) so http uploads land
    /// exactly where those consumers read.
    pub fn blob_handle(&self) -> crate::blobs::BlobHandle {
        self.blobs.clone()
    }

    /// a clone of the command lane's sender, for embedder-side producers
    /// that inject commands exactly as the http layer does — the oracle
    /// pool's completed provider runs re-enter as `Submit` commands here.
    pub fn command_sender(&self) -> mpsc::Sender<NodeCommand> {
        self.cmds.clone()
    }

    /// the `/v1/status` snapshot cell — the owning actor keeps a clone and
    /// publishes into it at every boundary it settles.
    pub fn status_cell(&self) -> StatusCell {
        self.status.clone()
    }

    /// the multiplexed stream hub backing `/v1/ws`.
    pub fn stream_hub(&self) -> StreamHub {
        self.hub.clone()
    }

    pub(crate) fn stream_index(&self) -> Option<Arc<indexer::IndexStore>> {
        self.index.clone()
    }

    /// Publish a durable shutdown state to every current and future surface.
    /// Request graceful shutdown; embedders (simnode lib) call this for teardown.
    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// resolves once a client asked the daemon to exit (POST /v1/admin/shutdown).
    pub async fn shutdown_requested(&self) {
        let mut shutdown = self.shutdown.subscribe();
        if *shutdown.borrow() {
            return;
        }
        let _ = shutdown.changed().await;
    }

    pub(crate) async fn send(&self, cmd: NodeCommand) -> Result<(), Response> {
        let mut cmds = self.cmds.clone();
        cmds.send(cmd)
            .await
            .map_err(|_| error_response(StatusCode::SERVICE_UNAVAILABLE, "node actor is gone"))
    }
}

/// the account `key` belongs to, read from committed identity state over the
/// command lane. Shared by the gates that admit only a key holding an account:
/// the huddle join and node-proof mint (`crate::call`) and the run-output
/// reader admission (`crate::stream`).
pub(crate) async fn account_of_key(
    handle: &NodeHandle,
    key: Vec<u8>,
) -> Result<Option<u64>, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    handle
        .send(NodeCommand::Query {
            target: "identity".into(),
            req: identity::encode_query(&identity::IdentityQuery::OfKey { key }),
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    // a node-internal read: the caller's own reason names this lookup, so the
    // refusal's token would only be shadowed by it. the sentence still travels.
    let bytes = rx
        .await
        .map_err(|_| "reply dropped".to_string())?
        .map_err(|refused| refused.message)?;
    let identity::IdentityReply::Account(account) = identity::decode_reply(&bytes)? else {
        return Err("unexpected identity reply".into());
    };
    Ok(account.map(|account| account.number))
}

#[cfg(test)]
mod refused_tests {
    use super::*;

    /// A module refusal reaches the receipt as the module's OWN token. The
    /// receipt's `reason` is what a caller branches on, so a stamp naming only
    /// the layer ("module") would tell it nothing it did not already know.
    #[test]
    fn a_module_refusal_carries_its_own_token_into_the_receipt() {
        let refused = Refused::of(&sdk::Error::module(
            "non_fast_forward",
            "forge HEAD moved; fetch and retry",
        ));
        assert_eq!(refused.reason, "non_fast_forward");
        assert_eq!(refused.message, "forge HEAD moved; fetch and retry");

        // a kernel refusal keeps its own class, unchanged by the module lane.
        let unknown = Refused::of(&sdk::Error::UnknownModule("nope".into()));
        assert_eq!(unknown.reason, "unknown_module");
    }

    /// The same refusal, but arriving as the ONE framed string a drained frame
    /// or a relayed rejection carries. It splits back into the same two halves,
    /// and a sentence that itself contains `": "` survives whole.
    #[test]
    fn a_framed_refusal_splits_back_into_its_token_and_sentence() {
        let sentence = "store-backed state keys: got 7 bytes";
        let framed = sdk::refusal::encode("state_key_shape", sentence);
        let refused = Refused::framed(&framed);
        assert_eq!(refused.reason, "state_key_shape");
        assert_eq!(refused.message, sentence);
    }

    /// A refusal that named no class is NOT given one. Inventing a token here
    /// would mint a word no module chose, which every consumer downstream would
    /// then have to tell apart from a real one.
    #[test]
    fn an_unframed_refusal_is_not_given_a_token() {
        for unframed in ["nobody framed this", "Not_Snake_Case: sentence"] {
            let refused = Refused::framed(unframed);
            assert_eq!(refused.reason, "unframed_refusal", "{unframed:?}");
            assert_eq!(refused.message, unframed);
        }
    }
}

#[cfg(test)]
mod status_cell_tests {
    use super::*;

    /// The chain id is a boot fact, not a boundary fact: a role loop publishes
    /// whole snapshots with an empty `chain_id`, and a reader still sees the
    /// wired one every time.
    #[test]
    fn a_wired_chain_id_survives_every_boundary_publish() {
        let cell = StatusCell::default();
        assert_eq!(cell.current().chain_id, "", "unwired: the published value");
        cell.wire_chain_id("mynet#d0cdf950".into());
        cell.publish(NodeStatus {
            version: "0.1.0".into(),
            chain_id: String::new(),
            ..Default::default()
        });
        assert_eq!(cell.current().chain_id, "mynet#d0cdf950");
        // the first boot fact wins.
        cell.wire_chain_id("other".into());
        assert_eq!(cell.current().chain_id, "mynet#d0cdf950");
    }

    /// A recovering node answers with the floor it recovered, not with zero.
    ///
    /// The boot publish is everything a node knows before it has read its own
    /// disk: build version and identity, and a zeroed boundary. The moment
    /// recovery names a floor, the two numbers a reader looks at first say so —
    /// otherwise a restart is indistinguishable from an empty network for the
    /// whole window, which on a release flip is every node in the fleet.
    #[test]
    fn a_recovered_floor_replaces_the_zeroed_boundary_before_consensus_resumes() {
        let cell = StatusCell::default();
        cell.publish(NodeStatus {
            contract: crate::NODE_CONTRACT,
            version: "0.1.0+97b4ef7bc".into(),
            public_key: "aa".into(),
            ..Default::default()
        });
        let booted = cell.current();
        assert_eq!((booted.height, booted.root_hash.as_str()), (0, ""));

        cell.publish_recovered(6546, "a411ddb91c18f309".into());

        let recovered = cell.current();
        assert_eq!(recovered.height, 6546);
        assert_eq!(recovered.root_hash, "a411ddb91c18f309");
        assert_eq!(
            recovered.version, "0.1.0+97b4ef7bc",
            "the recovered pair moves alone; the boot facts stand"
        );
        assert_eq!(recovered.public_key, "aa");
    }
}
