//! Embeddable deterministic sim node.
//!
//! [`boot`] composes the same node the `ducktape-simnode` binary serves —
//! noded's full `/v1` router plus the `/sim/*` determinism lane — on
//! caller-owned storage and a caller-chosen listen address (`:0` for
//! ephemeral), running on private background threads so an embedder needs no
//! tokio runtime. It returns a synchronous [`SimHandle`] (`step`, `set_auto`,
//! `peer_block`, `state`, `wait`, `shutdown`) and installs NO process-global
//! side effects unless `SimOpts::install_log` is set. The binary is a thin
//! `main` over this.
//!
//! the deterministic scenario node: noded's exact /v1 http+ws wire over a
//! test-scripted block producer.
//!
//! same two-runtime shape as noded — the (non-Send) `host::Host` lives on its
//! own actor thread inside a commonware runner, axum serves on the main
//! thread — but the actor answers to TWO channels: the [`NodeCommand`] lane
//! every client dials, and a `/sim/*` control lane only tests use. the
//! semantic difference is WHEN blocks commit:
//!
//!   - hold mode (default): a submit is queued and its http reply retained —
//!     the request hangs, exactly like the networked validator's held submit —
//!     until `POST /sim/step` commits the next queued op as one block.
//!   - auto mode (`--auto` or `POST /sim/auto`): every submit commits
//!     immediately and drains its worker follow-ups, i.e. noded's behavior.
//!
//! block time is a LOGICAL clock (`SIM_EPOCH_MS + height * SIM_BLOCK_MS`) —
//! the one wall-clock read in noded's block path (`consensus_time`) is what
//! made replays non-reproducible, so the same op script always produces the
//! same root-hash here. reads (query/status) always serve committed state:
//! held ops are invisible until stepped, which is consensus semantics.
//!
//! the sim actor IS an [`OrderedNode`]`<`[`StepOrderer`]`, `[`NullSink`]`>` —
//! the SAME apply/drain/projection engine the validator runs, with a scripted
//! FIFO orderer (`/sim/step` releases one; auto releases each) and a logical
//! [`ConsensusTimePolicy::Epoch`] clock. a commit is flush → step-release →
//! `drain_delivered` → [`noded::projection::project_block`] — one shared block
//! path, no re-implemented row/index assembly. `NullSink` keeps the no-WAL
//! restart-from-index-watermark behavior.
//!
//! the blocks/index lane is the real daemons' exactly: every commit feeds the
//! durable block index (`BlockOps.record` via the shared `project_block`), so
//! `GET /v1/blocks` and `/v1/index/*` serve just like noded. personas shape
//! the one wire difference left between the two real nodes — the receipt:
//!   - `local`: submit receipts carry `op_hash` (the embedded daemon's shape).
//!   - `networked`: receipts are height-only — a response layer strips
//!     `op_hash`, the validator's shape until its ordered-node convergence.
//!
//! `POST /sim/peer-block` commits a block owned by no held submit — the
//! "concurrent writer" for optimistic-projection race scenarios. it takes
//! EITHER the single-op `{target, payload, origin?}` shape OR a multi-op
//! `{ops: [{target, payload, origin?}, …]}` shape: the ops array commits ONE
//! block with N members through the host's `submit_block` batch engine (per-op
//! isolation, one shared root-hash), and the reply carries a per-member
//! applied/rejected verdict so a test can pin the host's abort-all-and-replay
//! member isolation.
//!
//! signed-frame lane: `POST /v1/submit/frame` verifies exactly as the real
//! daemon does. it decodes the raw frame bytes with `node::decode_frame` — the
//! SAME codec every validator uses — and commits under the frame's VERIFIED
//! signer as `Origin::External`, with the same hold/auto semantics as the
//! frameless lane (a frame parks like any submit in hold mode; auto commits +
//! drains). the signer key is self-authenticating: verifying a frame needs no
//! mesh, no dispatch pool, no provisioner, so this lane is honest here in a way
//! `/v1/call/ws` (which needs a peer) is not. this is the one lane where
//! authorship is CRYPTOGRAPHIC rather than the trusted-client string
//! convention.
//!
//! `--node-key <64-hex>` fabricates a mesh identity for consensus-op scenarios
//! (huddle membership names a node key): `status().public_key` serves it back
//! instead of the empty default. no mesh sits behind it — the sim routes no
//! peer traffic; it is a value for state ops to reference. a key that is not
//! 32 bytes of hex fails loud at startup.
//!
//! genesis composition: the sim runs the SAME composer bin/node does
//! (`noded::compose::compose` over a `topology` selection), so every wasm tenant
//! is the REAL guest component — read from `--modules <dir>`, defaulting to the
//! founding set the build staged beside this executable, so a bare checkout
//! that built the sim boots with nothing installed.
//!
//! opt-in governance genesis: `--with-valset <hex-pubkey>[,<hex>...]` (comma-
//! separated, and repeatable) appends the kv/valset/acl/governance/modules
//! system modules AFTER the default 16, seeding the validator set with the given
//! genesis ed25519 keys exactly like bin/node. `--invite-binding <string>`
//! (default `"sim"`, meaningful only with `--with-valset`) sets the network
//! binding governance verifies invite tokens against. registering the upgrade
//! module makes the host's once-per-block boundary `Advance` ride every sim
//! block automatically, and it is also the code registry governance is wired
//! to — so `UpdateModule` proposals are live here, not gated off.
//!
//! origin hex escape (sim lanes only): a `/v1/submit` or `/sim/peer-block`
//! origin string prefixed `hex:` (e.g. `hex:ab12…`, any even-length hex)
//! decodes to RAW bytes before becoming `Origin::External` — the only way to
//! author as a real 32-byte ed25519 key (governance ballots key on it, and raw
//! pubkey bytes are not valid UTF-8). malformed hex after the prefix is a hard
//! request reject, never a silent fall-through to the literal string.
//!
//! honesty rules: no synthetic-rejection knob (rejection scenarios must use
//! genuinely rejectable ops, so module semantics stay real), no live LLM
//! worker (an external llm call in a determinism tool is a contradiction; the
//! echo worker behind `--echo-oracle` is the only oracle). storage should be a
//! fresh dir per run (the height resumes above the index watermark like
//! noded's, but reused module state defeats the same-script reproducibility
//! this tool exists for).
//!
//! run: `cargo run -p simnode -- [--listen <addr>] [--storage <dir>]
//!       [--auto] [--persona local|networked] [--echo-oracle]
//!       [--with-valset <hex>[,<hex>...]] [--invite-binding <string>]
//!       [--node-key <64-hex>] [--modules <dir>]`
//! — `--listen` defaults to [`DEFAULT_LISTEN`].
//!
//! block-on-reject (validator parity): a rejected SINGLE op JOURNALS a block
//! here, exactly like the ordered validator — the op rides the drain, seals its
//! height with a `rejected` explorer row, and the submitter still gets the
//! rejection reply. (this replaced the old `Host::submit_at`-aborts-pre-commit
//! behavior where a rejected single op minted no block.) a rejected MEMBER of a
//! batch is folded into that batch's one block with its own `rejected` verdict,
//! as before.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use commonware_runtime::{Metrics as _, Runner as _, Supervisor as _};
use futures::StreamExt as _;
use futures::channel::{mpsc, oneshot};
use futures::select;
use host::BlockOp;
use host::worker;
use indexer::IndexStore;
use node::{ConsensusTimePolicy, DrainedFrame, NullSink, OrderedNode, StepHandle, StepOrderer};
use noded::bundle::{DirCodeSource, qmdb_stores};
use noded::compose::{Admissions, Bindings, Boot, Substrates, compose};
use noded::{
    BlockDisposition, BlockSummary, LOCAL_CHAIN_ID, ModuleCategory, ModuleStatus, NodeCommand,
    NodeHandle, NodeStatus, ORACLE_ORIGIN, StreamHub, hex_bytes, hex_root,
};
use sdk::{Event, Msg, Origin};
use serde::{Deserialize, Serialize};
use topology::TOPOLOGY;

// the sim's genesis sets are the `sim_base` (+ `sim_valset`) selections of the
// single-source `topology` — noded's exact 16-module default plus the
// opt-in 5 system modules. changing the daemon set changes the topology, which
// re-pins here and at node/demo. the composer below builds exactly those ids,
// the same way bin/node does.
const PEER_ORIGIN: &[u8] = b"peer";

/// where the binary serves `/v1` when `--listen` is absent — the ONE spelling
/// of the sim's default port.
///
/// It sits outside the real node's operator block (HTTP 8844, admin RPC 8845,
/// mesh 8846) on purpose, and that is a correctness property rather than
/// tidiness: the sim is a dev tool run BESIDE a node, so a shared port makes
/// the second process to boot die on its bind, and makes a client on that port
/// reach whichever daemon won — answering an admin-RPC caller with `/v1` http.
/// Keep any new default out of 8844..=8846; a test below pins it.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8850";

/// the logical clock: `consensus_time = SIM_EPOCH_MS + height * SIM_BLOCK_MS`.
/// a fixed epoch keeps module timestamps (message sent_at, task created_at)
/// plausible in the ui while staying identical across runs.
pub const SIM_EPOCH_MS: u64 = 1_750_000_000_000;
pub const SIM_BLOCK_MS: u64 = 1_000;

/// cap when buffering a /v1/submit response body to strip `op_hash` — receipts
/// are ~200 bytes; anything past this is not a receipt.
const RECEIPT_BODY_CAP: usize = 64 * 1024;

// ── Wire shapes of the /sim control lane ────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Persona {
    Local,
    Networked,
}

#[derive(Clone, Serialize)]
struct SimSnapshot {
    height: u64,
    held: usize,
    oracle_queued: usize,
    auto: bool,
    persona: Persona,
}

#[derive(Clone, Serialize)]
struct CommittedInfo {
    height: u64,
    root_hash: String,
    op_hash: String,
    target: String,
    /// `held` (a client submit released by this step), `oracle` (a worker
    /// follow-up).
    kind: &'static str,
}

#[derive(Serialize)]
struct StepReport {
    /// `null` when the step found both queues empty, or when the stepped op
    /// was rejected (its submitter got the rejection as its reply).
    committed: Option<CommittedInfo>,
    #[serde(flatten)]
    snapshot: SimSnapshot,
}

#[derive(Deserialize)]
struct AutoRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct PersonaRequest {
    persona: Persona,
}

/// one op inside a `/sim/peer-block` request's `ops` array.
#[derive(Deserialize)]
struct PeerOp {
    target: String,
    payload: serde_json::Value,
    origin: Option<String>,
}

/// a peer op's actor-lane fields: `(target, payload_bytes, origin_bytes)`. the
/// `hex:` origin escape is still unresolved here — the actor resolves it.
type PeerOpWire = (String, Vec<u8>, Vec<u8>);

/// `/sim/peer-block`: ONE shape — `{ops:[…]}`. a single-op peer block is a
/// one-element batch; the reply is always the per-member [`BatchInfo`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerBlockRequest {
    ops: Vec<PeerOp>,
}

/// one member's verdict in a `/sim/peer-block` batch reply (input order).
#[derive(Serialize)]
struct MemberInfo {
    target: String,
    /// this member's authored origin, hex or the printable convention — the
    /// same rendering the block row's `proposer` uses.
    proposer: String,
    /// `applied` (mutated state) or `rejected` (isolated, rolled back).
    disposition: BlockDisposition,
    /// the module's verbatim rejection reason; absent for an applied member.
    #[serde(skip_serializing_if = "Option::is_none")]
    rejection: Option<String>,
}

/// the multi-op `/sim/peer-block` reply: ONE committed block carrying N members,
/// each with its own applied/rejected verdict.
#[derive(Serialize)]
struct BatchInfo {
    height: u64,
    root_hash: String,
    /// one entry per input op, in input order.
    members: Vec<MemberInfo>,
}

// ── Control commands into the actor ─────────────────────

enum SimCommand {
    Step {
        reply: oneshot::Sender<StepReport>,
    },
    SetAuto {
        enabled: bool,
        reply: oneshot::Sender<SimSnapshot>,
    },
    SetPersona {
        persona: Persona,
        reply: oneshot::Sender<SimSnapshot>,
    },
    /// N ops committed as ONE block via the host's `submit_block` batch engine.
    /// each tuple is `(target, payload_bytes, origin_bytes)`; the `hex:` origin
    /// escape resolves in the actor.
    PeerBlock {
        ops: Vec<PeerOpWire>,
        reply: oneshot::Sender<Result<BatchInfo, String>>,
    },
    Snapshot {
        reply: oneshot::Sender<SimSnapshot>,
    },
}

/// the control lane's axum state: a sender into the actor. persona lives
/// separately (shared with the receipt-strip layer) because the middleware
/// must read it per-response without an actor round-trip. (the embedder-facing
/// [`SimHandle`] is a different, richer type — this one is only the /sim router's
/// state.)
#[derive(Clone)]
struct ControlState {
    control: mpsc::Sender<SimCommand>,
}

// ── Embeddable API ──────────────────────────────────────

/// how to compose the node [`boot`] serves — mirrors the binary's flags so an
/// embedder builds the same node without a command line. `Default` is the
/// binary's default composition EXCEPT `install_log`: the binary sets it, an
/// embedder must opt in (see the field).
pub struct SimOpts {
    /// auto mode: every submit commits immediately and drains its follow-ups
    /// (noded's behavior). `false` = hold mode — submits park until [`SimHandle::step`].
    pub auto: bool,
    /// register the deterministic echo oracle (`--echo-oracle`).
    pub echo_oracle: bool,
    /// opt-in governance genesis: raw 32-byte ed25519 validator pubkeys. empty
    /// => the default 16-module set (`topology::SIM_BASE`) alone.
    pub valset_keys: Vec<Vec<u8>>,
    /// the invite namespace governance verifies tokens against — meaningful only
    /// with `valset_keys`. defaults to `b"sim"`.
    pub invite_binding: Vec<u8>,
    /// fabricate a mesh identity `status().public_key` serves: a canonical
    /// lowercase-hex ed25519 pubkey (the binary validates 32 bytes at parse).
    /// `None` => empty (no peer-routed features).
    pub node_key: Option<String>,
    /// the receipt/ring persona — local daemon vs networked validator shapes.
    pub persona: Persona,
    /// the directory of `<id>.component.wasm` the sim composes its wasm tenants
    /// from; `None` = the repo's kernel fixtures, so an embedder in this
    /// checkout needs no install.
    pub modules_dir: Option<PathBuf>,
    /// install noded's PROCESS-GLOBAL tracing subscriber + panic hook (feeding
    /// the log ring). the binary sets this; an embedder leaves it `false` so
    /// `boot` has no global side effects and repeated boots don't stack hooks.
    pub install_log: bool,
}

impl Default for SimOpts {
    fn default() -> Self {
        Self {
            auto: false,
            echo_oracle: false,
            valset_keys: Vec::new(),
            invite_binding: b"sim".to_vec(),
            node_key: None,
            persona: Persona::Local,
            modules_dir: None,
            install_log: false,
        }
    }
}

/// compose and start the sim node on `storage` and `listen` (`:0` for an
/// ephemeral port — [`SimHandle::addr`] reports the real one). the block-
/// producing actor and the `/v1` + `/sim` serve loop run on private background
/// threads, so the caller needs no tokio runtime. returns once the listener is
/// bound (`/v1/status` is live). the only process-global side effect is the
/// tracing subscriber, and only under `opts.install_log`.
pub fn boot(storage: &Path, listen: SocketAddr, opts: SimOpts) -> Result<SimHandle, String> {
    let SimOpts {
        auto,
        echo_oracle,
        valset_keys,
        invite_binding,
        node_key,
        persona,
        modules_dir,
        install_log,
    } = opts;
    // opt-in wasmtime compilation cache for the sim/test lane only: the sim
    // suite's 13-plus test binaries all compile the same genesis, so sharing
    // a cache dir across them turns wall time way down. A real node binary
    // never calls this hook, so no environment variable can turn the cache
    // on in production.
    if let Some(dir) = std::env::var_os("DUCKTAPE_WASM_CACHE_DIR") {
        wasm_host::enable_compilation_cache(std::path::PathBuf::from(dir));
    }
    // the founding set every wasm tenant (and every index guest) composes
    // from. the default is the set the build staged beside this executable —
    // the sim is a dev tool that must boot from a bare checkout, and a bare
    // checkout that built it has that set.
    let modules_dir = match modules_dir {
        Some(dir) => dir,
        None => workspace_config::sim_modules_dir()?,
    };

    // the status module list and the index tier both extend only under valset
    // keys; the default path stays the exact 16-module set `daemon_e2e` pins
    // against noded.
    let module_ids: Vec<&'static str> = if valset_keys.is_empty() {
        topology::SIM_BASE.to_vec()
    } else {
        topology::SIM_BASE
            .iter()
            .chain(topology::SIM_VALSET)
            .copied()
            .collect()
    };
    // read and hash the bundle HERE, in the caller: ONE decision about whether
    // the components are usable, surfaced as `boot`'s own Err instead of a
    // panic on the actor thread that reaches an embedder as "sim actor died
    // during genesis". the source (a path and a hash table) then rides in.
    let wasm_ids = TOPOLOGY.wasm_ids(&module_ids);
    let (code, code_hashes) = DirCodeSource::open(&modules_dir, &wasm_ids)?;

    let storage = storage.to_path_buf();
    let forge_repo = storage.join("forge-git");

    // the durable block index: /v1/blocks and /v1/index/* read it, the sim
    // actor feeds it block-by-block — noded's construction site verbatim, so
    // the sim runs the SAME wasm index guests as the real daemons, from the
    // same founding set its components came from.
    let index = noded::open_index_store(&storage, &module_ids)?;

    // the log ring is a process-GLOBAL subscriber (and stacks a panic hook per
    // call), so wire it ONLY under `install_log` — the binary does; an embedder
    // running several sims in-process must not. without it the handle still owns
    // its own ring, nothing just feeds the global tracing layer.
    let (handle, cmd_rx, stream_hub) = if install_log {
        let log_ring = noded::LogRing::default();
        noded::log::init(Some(log_ring.clone()), None);
        NodeHandle::channel_with_log_ring(log_ring)
    } else {
        NodeHandle::channel()
    };
    // the sim serves noded's router VERBATIM, admin namespace included, so it
    // needs the same operator credential the real daemons mint — without one
    // `admit_operator` refuses every request with `operator_token_unavailable`
    // and the binary has no HTTP stop path at all (`sim_router` has no shutdown
    // of its own; `SimHandle::wait` waits on exactly `/v1/admin/shutdown`).
    // Minted into `storage`, so a driver reads it back with
    // `noded::admin::read_operator_token(storage)`, and so does the harness for
    // its mutating `/v1` writes. `DUCKTAPE_ADMIN=off` still removes the control
    // surface entirely (the credential is minted regardless); a mint failure
    // refuses every admin request rather than falling back to loopback trust.
    let handle = handle
        .with_forge_repo(forge_repo.clone())
        .with_index_store(index.clone())
        .with_admin(noded::AdminConfig::minted(
            noded::AdminExposure::from_env(),
            &storage,
        ));

    let (control_tx, control_rx) = mpsc::channel::<SimCommand>(16);
    let persona = Arc::new(Mutex::new(persona));
    let fatal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let public_key = node_key.unwrap_or_default();

    // the readiness event: the actor publishes its FIRST status snapshot
    // (genesis or the resumed height) and signals; the http listener is not
    // bound until then. `/v1/status` answering 200 is what every client uses as
    // "this sim is up" — the harness, the CLI, an embedder — so serving before
    // the actor's boot publish hands them a snapshot claiming version "",
    // root_hash "", no modules, height 0 and an empty public_key even when
    // `--node-key` seeded one. On a restart over existing storage that height
    // is an outright lie about committed state. noded's `booted` signal exists
    // for exactly this; the sim serves noded's router, so it owes the same
    // contract.
    let (booted_tx, booted_rx) = std::sync::mpsc::sync_channel::<()>(1);

    // the actor: owns the non-Send host on its own commonware runner thread,
    // answering the node command lane + the /sim control lane. it holds a node
    // clone (for FATAL request_shutdown) and the shared fatal flag.
    let actor_storage = storage.clone();
    let actor_persona = persona.clone();
    let actor_node = handle.clone();
    let actor_fatal = fatal.clone();
    let blobs = handle.blob_handle();
    let actor = std::thread::Builder::new()
        .name("sim-actor".into())
        .spawn(move || {
            run_sim(
                actor_storage,
                forge_repo,
                code,
                code_hashes,
                index,
                blobs,
                actor_persona,
                auto,
                echo_oracle,
                valset_keys,
                invite_binding,
                public_key,
                cmd_rx,
                control_rx,
                stream_hub,
                actor_node,
                actor_fatal,
                booted_tx,
            )
        })
        .map_err(|err| format!("spawn sim-actor thread: {err}"))?;

    // a dropped sender means the actor thread died inside genesis — report that
    // instead of binding a listener that would serve the empty default forever.
    booted_rx
        .recv()
        .map_err(|_| "sim actor died during genesis — see the error above".to_string())?;

    // the serve loop on a private 2-worker tokio runtime, so an embedder needs
    // no runtime of its own. it binds, reports the REAL bound addr back over a
    // std channel (so `:0` resolves before boot returns), then serves until
    // shutdown_requested.
    let router_control = control_tx.clone();
    let serve_handle = handle.clone();
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<Result<SocketAddr, String>>();
    let serve = std::thread::Builder::new()
        .name("sim-serve".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = addr_tx.send(Err(format!("build serve runtime: {err}")));
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::bind(listen).await {
                    Ok(listener) => listener,
                    Err(err) => {
                        let _ = addr_tx.send(Err(format!("bind {listen}: {err}")));
                        return;
                    }
                };
                let bound = match listener.local_addr() {
                    Ok(bound) => bound,
                    Err(err) => {
                        let _ = addr_tx.send(Err(format!("read bound addr: {err}")));
                        return;
                    }
                };
                let shutdown = serve_handle.clone();
                let app = noded::router(serve_handle)
                    .merge(sim_router(ControlState {
                        control: router_control,
                    }))
                    .layer(axum::middleware::from_fn_with_state(
                        persona,
                        strip_receipt_op_hash,
                    ));
                // report the bound addr only once the app is built and about to
                // serve — boot returns after this, so /v1 answers immediately.
                let _ = addr_tx.send(Ok(bound));
                // connect-info so the admin namespace's fail-closed loopback gate
                // sees the (loopback) peer — same as noded::serve.
                let served = axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move { shutdown.shutdown_requested().await })
                .await;
                if let Err(err) = served {
                    tracing::error!(
                        target: "ducktape::node",
                        error = %err,
                        "sim serve loop exited with error"
                    );
                }
            });
        })
        .map_err(|err| format!("spawn sim-serve thread: {err}"))?;

    // block until the serve thread reports its bound addr (or a bind failure). a
    // dropped sender means the thread died before binding. on any error the
    // spawned actor exits on its own: both control senders (this local and the
    // serve thread's) drop, closing its control lane.
    let addr = addr_rx
        .recv()
        .map_err(|_| "serve thread exited before binding".to_string())??;

    Ok(SimHandle {
        addr,
        control: Some(control_tx),
        node: handle,
        serve: Some(serve),
        actor: Some(actor),
        fatal,
    })
}

/// a running sim node, embeddable without a tokio runtime. synchronous: each
/// method sends one control command to the actor and blocks for its reply. the
/// serve loop and actor run on private threads this handle owns — dropping it
/// (or [`Self::shutdown`]) tears both down cleanly. [`Self::wait`] is the
/// binary's path: block until the node stops, surfacing any fatal reason.
pub struct SimHandle {
    addr: SocketAddr,
    /// the /sim control lane into the actor. `Option` so teardown can drop the
    /// last sender (closing the actor's control channel) without moving out of a
    /// `Drop` type. `None` once torn down — calls then return `Err`.
    control: Option<mpsc::Sender<SimCommand>>,
    /// held to request graceful shutdown on teardown (and matched by the actor's
    /// own clone that a FATAL commit fires).
    node: NodeHandle,
    serve: Option<std::thread::JoinHandle<()>>,
    actor: Option<std::thread::JoinHandle<()>>,
    fatal: Arc<Mutex<Option<String>>>,
}

impl SimHandle {
    /// the real bound listen address (`:0` resolved to a concrete port).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// commit exactly one queued op — an oracle follow-up first, else the oldest
    /// held submit. the [`StepReport`] as JSON, the shape `POST /sim/step` returns.
    pub fn step(&self) -> Result<serde_json::Value, String> {
        let report: StepReport = self.call(|reply| SimCommand::Step { reply })?;
        serde_json::to_value(report).map_err(|err| err.to_string())
    }

    /// toggle auto mode; entering auto flushes the parked backlog.
    pub fn set_auto(&self, enabled: bool) -> Result<(), String> {
        let _: SimSnapshot = self.call(|reply| SimCommand::SetAuto { enabled, reply })?;
        Ok(())
    }

    /// commit a concurrent writer's block past any parked queue. `body` is the
    /// `/sim/peer-block` request shape (`{ops:[…]}`); the reply is the
    /// per-member `BatchInfo`.
    pub fn peer_block(&self, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let request: PeerBlockRequest =
            serde_json::from_value(body).map_err(|err| err.to_string())?;
        let mut encoded = Vec::with_capacity(request.ops.len());
        for op in request.ops {
            encoded.push(encode_peer_op(op).map_err(|(_, msg)| msg)?);
        }
        let info = self.call(|reply| SimCommand::PeerBlock {
            ops: encoded,
            reply,
        })??;
        serde_json::to_value(info).map_err(|err| err.to_string())
    }

    /// the current [`SimSnapshot`] as JSON, the shape `GET /sim/state` returns.
    pub fn state(&self) -> Result<serde_json::Value, String> {
        let snapshot: SimSnapshot = self.call(|reply| SimCommand::Snapshot { reply })?;
        serde_json::to_value(snapshot).map_err(|err| err.to_string())
    }

    /// the binary's path: block until the node stops — an external
    /// `/v1/admin/shutdown`, or a FATAL commit that halted it — then join both
    /// threads. `Err(reason)` if a fatal was recorded (`main` turns that into
    /// exit 1). does NOT itself request shutdown, so a normal binary serves
    /// until killed.
    pub fn wait(mut self) -> Result<(), String> {
        self.join_threads();
        match self.fatal.lock().expect("fatal flag poisoned").clone() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// tear the node down: request graceful shutdown, then join both threads.
    pub fn shutdown(mut self) {
        self.node.request_shutdown();
        self.join_threads();
        // once both sim threads have exited, this handle's StatusCell
        // exposition closure owns the LAST commonware-executor ref, and
        // dropping the tokio runtime inside it panics on an async embedder
        // thread (the app's current_thread tests) — hand the final drop to a
        // scratch thread and wait for it.
        let reaper = std::thread::Builder::new()
            .name("sim-drop".into())
            .spawn(move || drop(self));
        if let Ok(reaper) = reaper {
            let _ = reaper.join();
        }
    }

    /// send one control command and block for its reply. a torn-down actor is
    /// `Err("sim actor stopped")` — the sync twin of the /sim router's 503.
    fn call<T, F>(&self, build: F) -> Result<T, String>
    where
        F: FnOnce(oneshot::Sender<T>) -> SimCommand,
    {
        // fail CLOSED once a fatal commit halted the host. `halt` tears down the
        // serve loop but not the actor (this live handle still holds a control
        // sender), so without this guard a post-fatal step/state/set_auto/
        // peer_block would answer Ok on a corrupt host — every embedded method
        // routes through here, so one check covers all. `wait` surfaces the same
        // reason for the binary path.
        if let Some(reason) = self.fatal.lock().expect("fatal flag poisoned").clone() {
            return Err(reason);
        }
        let mut sender = self
            .control
            .clone()
            .ok_or_else(|| "sim actor stopped".to_string())?;
        let (reply_tx, reply_rx) = oneshot::channel();
        sender
            .try_send(build(reply_tx))
            .map_err(|_| "sim actor stopped".to_string())?;
        futures::executor::block_on(reply_rx).map_err(|_| "sim actor stopped".to_string())
    }

    /// join serve then actor, idempotently. the serve thread ends on
    /// shutdown_requested (dropping its command + control senders); dropping OUR
    /// control sender then closes the actor's control channel so its `select!`
    /// breaks. a test process must exit cleanly, so neither thread is leaked.
    fn join_threads(&mut self) {
        if let Some(serve) = self.serve.take() {
            let _ = serve.join();
        }
        // drop the last /sim control sender so the actor's control lane closes.
        self.control = None;
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        self.node.request_shutdown();
        self.join_threads();
    }
}

// ── The actor ───────────────────────────────────────────

/// a client submit parked until a step commits it: the retained reply is what
/// keeps its http request hanging — the held-submit semantics under test.
struct HeldOp {
    origin: Origin,
    msg: Msg,
    reply: oneshot::Sender<Result<BlockSummary, String>>,
}

struct Sim {
    /// the ordered apply lane: the SAME `OrderedNode` drain the validator runs,
    /// over a scripted [`StepOrderer`] and the sim's logical-clock time policy.
    /// a commit is `submit_decoded` → `flush_batch` → step-release → drain →
    /// `project_block`. `NullSink` = no WAL (restart = qmdb + index watermark).
    node: OrderedNode<StepOrderer, NullSink>,
    /// releases parked frames into the drain: one per `/sim/step`, `batches`
    /// per immediate (peer / auto) commit. clones live on the actor only — the
    /// serve loop scripts releases through the control lane, not this handle.
    step: StepHandle,
    /// the index watermark the node resumed at (fresh = 0) — the height
    /// reported before the first block seals, when `node.finalized()` is `None`.
    resume_height: u64,
    auto: bool,
    persona: Arc<Mutex<Persona>>,
    held: VecDeque<HeldOp>,
    /// worker follow-ups awaiting commits. steps drain this BEFORE the next
    /// held submit — noded drains a submit's follow-ups to completion before
    /// touching the next command, and step order mirrors that.
    oracle_queue: VecDeque<Msg>,
    workers: Vec<Box<dyn worker::Worker>>,
    blobs: blobstore::BlobHandle,
    index: Arc<IndexStore>,
    stream_hub: StreamHub,
    /// the fabricated mesh identity `status` reports (`--node-key`), or empty
    /// for the default "no peer-routed features here". no mesh sits behind it.
    public_key: String,
    /// a clone of the node handle, held only so a FATAL commit can request
    /// graceful shutdown (the embeddable replacement for the binary's
    /// `process::exit(1)` — a lib must never kill the host process).
    handle: NodeHandle,
    /// set to the fatal reason if a commit hits a boundary fault. the embedder's
    /// [`SimHandle::wait`] surfaces it (the binary turns that into exit 1);
    /// reads/steps then fail because the actor is torn down.
    fatal: Arc<Mutex<Option<String>>>,
}

#[allow(clippy::too_many_arguments)]
fn run_sim(
    storage: PathBuf,
    forge_repo: PathBuf,
    code: DirCodeSource,
    code_hashes: BTreeMap<String, [u8; 32]>,
    index: Arc<IndexStore>,
    blobs: blobstore::BlobHandle,
    persona: Arc<Mutex<Persona>>,
    auto: bool,
    echo_oracle: bool,
    valset_keys: Vec<Vec<u8>>,
    invite_binding: Vec<u8>,
    public_key: String,
    mut cmds: mpsc::Receiver<NodeCommand>,
    mut control: mpsc::Receiver<SimCommand>,
    stream_hub: StreamHub,
    handle: NodeHandle,
    fatal: Arc<Mutex<Option<String>>>,
    // fired once, right after the boot snapshot is published — `boot` binds
    // the listener only then, so `/v1/status` never serves the empty default.
    booted: std::sync::mpsc::SyncSender<()>,
) {
    let duckfs_dir = storage.join("duckfs");
    let rt_cfg = commonware_runtime::tokio::Config::default().with_storage_directory(storage);
    let executor = commonware_runtime::tokio::Runner::new(rt_cfg);

    executor.start(|context| async move {
        // genesis: the topology selection composed the way bin/node composes —
        // wasm tenants fetched by hash from the source `boot` opened, the native
        // registries seeded from the sim's bindings. governance composes as
        // wasm, so its code registry is wired and UpdateModule proposals are
        // live in the sim.
        let substrates = Substrates {
            directory: duckfs_dir.with_file_name("module-storage"),
            bindings: [("forge".into(), forge_repo), ("files".into(), duckfs_dir)].into(),
            blobs: blobs.clone(),
        };
        let bindings = Bindings {
            invite: &invite_binding,
            chain_id: LOCAL_CHAIN_ID,
            // the sim arms `ConsensusTimePolicy::Epoch`, so its `consensus_time`
            // is a millisecond clock, not a height.
            time_unit: sdk::genesis_config::TimeUnit::Millis,
        };
        let mut stores = qmdb_stores(&context);
        let mut host = compose(
            &code,
            &mut stores,
            &substrates,
            &bindings,
            Boot::Genesis {
                validators: &valset_keys,
                bundle: &code_hashes,
            },
        )
        .await
        .expect("sim genesis composes");
        host.set_module_factory(Box::new(Admissions::new(&context, &substrates, &bindings)));
        noded::converge_host_modules(&index, &host).expect("deployed index guests converge");

        // a lib must not write to stdout — this is a once-per-boot lifecycle
        // fact, so it rides tracing (visible on the binary's stderr + ring under
        // `install_log`, silent for an embedder that opted out). no sim harness
        // reads this line off stdout; readiness is `/v1/status`.
        tracing::info!(
            target: "ducktape::consensus",
            root_hash = %hex_root(&host.root_hash()),
            "genesis"
        );

        // resume above the index watermark like noded — with the contractual
        // fresh dir this is 0; on a (discouraged) reused dir it keeps op-log
        // heights monotonic instead of silently skipping every new block.
        let resume_height = index.resume_height().expect("read index watermarks");
        stream_hub.prime(resume_height, hex_root(&host.root_hash()));

        // wrap the host on the ordered lane, over the scripted FIFO orderer.
        // `view_base = resume_height + 1` bases the first drained block (engine
        // view 0) at the height after the watermark — 1 on a fresh dir, so
        // genesis stays height 0 and blocks are 1-indexed exactly like before.
        // NullSink: no journal (restart = qmdb + index watermark). the sim's
        // logical clock rides the `Epoch` time policy — the drain stamps
        // `consensus_time = SIM_EPOCH_MS + height * SIM_BLOCK_MS` per block, the
        // byte-identical reproduction of the old hand-rolled clock.
        let (orderer, step) = StepOrderer::new();
        let mut node = OrderedNode::resume(host, orderer, NullSink, None, resume_height + 1);
        node.set_consensus_time_policy(ConsensusTimePolicy::Epoch {
            base_ms: SIM_EPOCH_MS,
            block_ms: SIM_BLOCK_MS,
        });

        let mut sim = Sim {
            node,
            step,
            resume_height,
            auto,
            persona,
            held: VecDeque::new(),
            oracle_queue: VecDeque::new(),
            workers: if echo_oracle {
                vec![Box::new(EchoWorker)]
            } else {
                Vec::new()
            },
            blobs,
            index,
            stream_hub,
            public_key,
            handle,
            fatal,
        };

        // the boot snapshot: /v1/status answers from the cell before the
        // first command, exactly like the real daemons. the exposition source
        // feeds /metrics + /v1/peers off-lane (no mesh: the sample parses
        // honestly empty, roles stay absent). `Context` has no Clone; a child
        // shares the SAME registry, so its encode() serves the identical
        // exposition.
        let exposition_context = context.child("exposition");
        sim.handle
            .status_cell()
            .wire_exposition(move || exposition_context.encode());
        sim.publish_status();
        // the snapshot is live: `boot` may bind the listener now. sending after
        // the publish is what makes "/v1/status never answers the empty
        // default" true — the whole point of the signal.
        let _ = booted.send(());
        loop {
            select! {
                cmd = control.next() => match cmd {
                    Some(cmd) => sim.handle_control(cmd).await,
                    None => break,
                },
                cmd = cmds.next() => match cmd {
                    Some(NodeCommand::Submit { target, payload, required_blob: _, origin, reply }) => {
                        // the `hex:` origin escape resolves to raw bytes here, so a
                        // client can author as a real ed25519 key; malformed hex is
                        // a hard reject, never a literal-string fall-through.
                        match decode_origin(origin) {
                            Ok(origin) => {
                                sim.handle_submit(
                                    Origin::External(origin),
                                    Msg { target, payload },
                                    reply,
                                )
                                .await;
                            }
                            Err(err) => {
                                let _ = reply.send(Err(err));
                            }
                        }
                    }
                    // the signed-frame lane, FAITHFUL to the real daemon: decode
                    // and verify the frame with the same `node::decode_frame` every
                    // validator uses, then hold/commit under the frame's VERIFIED
                    // signer as origin. the signer key is self-authenticating — no
                    // mesh, dispatch pool, or provisioner is needed to check it — so
                    // this lane is honest here in a way `/v1/call/ws` (which needs a
                    // peer) is not. junk never reaches the host: the http gate
                    // already refused it, and this decode is the second wall.
                    Some(NodeCommand::SubmitFrame { frame, reply }) => {
                        match node::decode_frame(&frame) {
                            Ok((origin, msg)) => {
                                sim.handle_submit(origin, msg, reply).await;
                            }
                            Err(err) => {
                                let _ = reply.send(Err(err.to_string()));
                            }
                        }
                    }
                    Some(NodeCommand::Query { target, req, reply }) => {
                        // reads serve COMMITTED state — the ordered lane applies
                        // only in `drain_delivered`, so a held/parked op is
                        // invisible here until a step commits it.
                        let result =
                            sim.node.host().query(&target, &req).await.map_err(|err| err.to_string());
                        let _ = reply.send(result);
                    }
                    Some(NodeCommand::QueryAs { target, req, reader, reply }) => {
                        let result = sim
                            .node
                            .host()
                            .query_as(&target, &req, sdk::Origin::External(reader))
                            .await
                            .map_err(|err| err.to_string());
                        let _ = reply.send(result);
                    }
                    None => break,
                },
            }
        }
    });
}

impl Sim {
    /// hold or commit ONE op under an already-resolved `Origin` — the shared
    /// lane behind both `/v1/submit` (a caller string, or the `hex:` escape) and
    /// `/v1/submit/frame` (the frame's cryptographically VERIFIED signer). the
    /// two lanes differ ONLY in how the origin was obtained; hold/auto semantics
    /// are identical past this point.
    async fn handle_submit(
        &mut self,
        origin: Origin,
        msg: Msg,
        reply: oneshot::Sender<Result<BlockSummary, String>>,
    ) {
        if !self.auto {
            self.held.push_back(HeldOp { origin, msg, reply });
            return;
        }
        // auto mode = noded's submit_and_drain: commit the caller's op as its own
        // block, then settle its worker follow-ups through the SHARED reactor loop
        // (each its own block). a rejected op still journals its block (validator
        // parity) and the submitter gets the rejection — no follow-ups to drain.
        let result = match self.commit_block(vec![(origin, msg)]).await {
            Ok((drained, events)) => match Self::member_summary(&drained) {
                Ok(block) => self.drive_auto(events).await.map(|()| block),
                Err(reason) => Err(reason),
            },
            Err(reason) => Err(reason), // fatal — the sim halted
        };
        let _ = reply.send(result); // caller may have hung up
    }

    async fn handle_control(&mut self, cmd: SimCommand) {
        match cmd {
            SimCommand::Step { reply } => {
                let committed = self.step_once().await;
                let _ = reply.send(StepReport {
                    committed,
                    snapshot: self.snapshot(),
                });
            }
            SimCommand::SetAuto { enabled, reply } => {
                self.auto = enabled;
                // entering auto flushes the backlog: every held op (and the
                // follow-ups it spawns) commits now, so "auto" always means
                // "nothing is parked".
                if enabled {
                    while self.step_once().await.is_some()
                        || !(self.held.is_empty() && self.oracle_queue.is_empty())
                    {
                    }
                }
                let _ = reply.send(self.snapshot());
            }
            SimCommand::SetPersona { persona, reply } => {
                *self.persona.lock().expect("persona poisoned") = persona;
                let _ = reply.send(self.snapshot());
            }
            SimCommand::PeerBlock { ops, reply } => {
                // resolve each member's origin through the SAME `hex:` escape as
                // /v1/submit — a concurrent writer can also author as a raw
                // ed25519 key; one bad origin rejects the whole request (before
                // any state moves — nothing has committed yet).
                let resolved: Result<Vec<(Origin, Msg)>, String> = ops
                    .into_iter()
                    .map(|(target, payload, origin)| {
                        decode_origin(origin)
                            .map(|o| (Origin::External(o), Msg { target, payload }))
                    })
                    .collect();
                let result = match resolved {
                    Ok(ops) => self.commit_peer_block(ops).await,
                    Err(err) => Err(err),
                };
                let _ = reply.send(result);
            }
            SimCommand::Snapshot { reply } => {
                let _ = reply.send(self.snapshot());
            }
        }
    }

    /// commit exactly one queued op — a pending oracle follow-up first, else
    /// the oldest held submit (releasing its receipt). `None` when idle, and
    /// `None` when the stepped op was rejected (the submitter got the rejection
    /// reply; the block is still journaled — validator parity — but the step
    /// reports no commit).
    async fn step_once(&mut self) -> Option<CommittedInfo> {
        // oracle follow-ups drain BEFORE held submits — noded settles a submit's
        // follow-ups before the next command, and step order mirrors that.
        if let Some(follow) = self.oracle_queue.pop_front() {
            let (drained, events) = self
                .commit_block(vec![(Origin::External(ORACLE_ORIGIN.to_vec()), follow)])
                .await
                .ok()?; // fatal — the sim halted, reads fail closed
            self.offer(&drained, events).await;
            return self.committed_info(&drained, "oracle").or_else(|| {
                tracing::warn!(
                    target: "ducktape::modules",
                    "worker follow-up REJECTED — the oracle's result never landed"
                );
                None
            });
        }
        let held = self.held.pop_front()?;
        let (drained, events) = match self.commit_block(vec![(held.origin, held.msg)]).await {
            Ok(out) => out,
            Err(reason) => {
                // fatal: the sim halted; surface it to the parked submitter.
                let _ = held.reply.send(Err(reason));
                return None;
            }
        };
        self.offer(&drained, events).await;
        // release the parked http reply with the op's consensus fate: an applied
        // block summary, or the module's rejection.
        let _ = held.reply.send(Self::member_summary(&drained));
        self.committed_info(&drained, "held")
    }

    /// commit N ops as ONE concurrent-writer block, returning per-member
    /// verdicts: `submit_decoded` each, flush into ONE batch (one block, one
    /// root-hash, per-op isolation), and read each member's applied/rejected
    /// disposition from the drain — the shared `project_block` already wrote
    /// the block's one row (all members, each with its disposition) and the
    /// per-module index feed. an empty `ops` produces no ordered frame and so
    /// no block. a rejected member still journals its block (validator parity).
    async fn commit_peer_block(&mut self, ops: Vec<(Origin, Msg)>) -> Result<BatchInfo, String> {
        let (drained, events) = self.commit_block(ops).await?;
        self.settle(&drained, events).await;
        // the batch is ONE block: every member frame shares its height and the
        // one post-batch root-hash the drain sealed.
        let members = drained
            .iter()
            .filter_map(|d| {
                let op = d.op.as_ref()?;
                let disposition = block_disposition(d.disposition)?;
                Some(MemberInfo {
                    target: op.target.clone(),
                    proposer: proposer_hex(&op.origin),
                    disposition,
                    rejection: d.reason.clone(),
                })
            })
            .collect();
        Ok(BatchInfo {
            height: self.height(),
            root_hash: hex_root(&self.node.root_hash()),
            members,
        })
    }

    /// commit N pre-resolved ops as ONE block on the ordered lane, feed the
    /// index + stream from the SHARED [`noded::projection::project_block`] seam,
    /// and return the per-op drained outcomes (input order) plus the block's
    /// emitted events. `submit_decoded` parks each op (the unsigned sim lanes
    /// never sign — no wire variant, the codec stays a machine contract);
    /// `flush_batch` packs them into batch super-frames; the [`StepHandle`]
    /// releases exactly those; `drain_delivered` applies them as blocks. a member
    /// REJECTION is a normal `Rejected` frame in the returned vec — the block
    /// STILL seals (validator parity). only a FATAL boundary fault halts the sim
    /// and returns `Err(reason)`.
    async fn commit_block(
        &mut self,
        ops: Vec<(Origin, Msg)>,
    ) -> Result<(Vec<DrainedFrame>, Vec<Event>), String> {
        for (origin, msg) in ops {
            self.node.submit_decoded(BlockOp {
                origin,
                msg,
                frame: [0u8; 32],
            });
        }
        // flush → release → drain are paired so the FIFO orderer never holds an
        // unreleased backlog: auto and hold differ only in WHEN commit_block is
        // called, never in this release.
        let batches = match self.node.flush_batch().await {
            Ok(batches) => batches,
            Err(err) => return Err(self.fatal(err)),
        };
        self.step.release(batches as u64);
        if let Err(err) = self.node.drain_delivered().await {
            return Err(self.fatal(err));
        }
        let drained = self.node.take_drained();
        let system = self.node.take_system_dispatches();
        // ONE block per drained height: feed the durable index (explorer row +
        // per-module dispatch feed) and publish the ws block frame. canonical
        // state is already sealed, so an index failure degrades the read models,
        // never the block. the row carries the ordered-frame id as its `hash`
        // exactly like the validator — the sim now frames its ops on the ordered
        // lane, so `project_block` fills it (where the old direct-host path left
        // it empty).
        for projection in noded::projection::project_block(&drained, system, &self.blobs) {
            // a height that sealed nothing is not a block: never fold one (see
            // `BlockProjection::sealed`).
            if !projection.sealed() {
                continue;
            }
            let time = ConsensusTimePolicy::Epoch {
                base_ms: SIM_EPOCH_MS,
                block_ms: SIM_BLOCK_MS,
            }
            .stamp(projection.height);
            noded::projection::apply_block_to_index(
                &self.index,
                projection.height,
                time,
                projection.record,
                &projection.dispatches,
                self.node.host(),
            );
            if let Some(root_hash) = projection.sealed_hash {
                self.stream_hub.publish_block(
                    projection.height,
                    hex_root(&root_hash),
                    noded::BlockWake::from_dispatches(&projection.dispatches),
                );
            }
        }
        // the deterministic lane's read barrier: fold triggers drain on a
        // background runner, and a sim commit must imply the derived views
        // answer the block (a client read — or the ws `changed` event that
        // prompts one — must never race the fold). production daemons stay
        // async by design; their clients re-read on the next event.
        if let Err(err) = self.index.wait_folds_drained() {
            tracing::error!(
                target: "ducktape::consensus",
                event = "node_index_poisoned",
                error = %err,
                "module index fold failed — the sim's views are now STALE"
            );
        }
        // the status snapshot is part of committing, for the same reason the
        // fold barrier above is: a commit must imply every read surface already
        // answers the block. this used to fire once per pump turn instead —
        // AFTER the arm had already sent its reply — so a caller that read
        // /v1/status the instant its receipt for height N arrived could still be
        // told N-1. publishing here makes that unobservable by construction,
        // rather than by winning a race.
        self.publish_status();
        Ok((drained, self.node.take_events()))
    }

    /// settle worker follow-ups in AUTO mode through the SHARED reactor loop
    /// ([`worker::drive`]): each follow-up commits as its own block, its events
    /// feed the next round, a stranded dispatch mailbox is nudged, and a
    /// self-retriggering worker is bounded. workers are moved out for the borrow
    /// (the lane holds `&mut self`).
    async fn drive_auto(&mut self, initial: Vec<Event>) -> Result<(), String> {
        let workers = std::mem::take(&mut self.workers);
        let result = {
            let mut lane = AutoLane { sim: self };
            worker::drive(&workers, initial, &mut lane).await
        };
        self.workers = workers;
        match result {
            Ok(unclaimed) => {
                let mut notes = noded::log::ModuleNotes::new(self.height());
                for eff in &unclaimed {
                    notes.unclaimed(eff);
                }
                notes.finish();
                Ok(())
            }
            // a budget-exceeded or halted-fatal loop: the sim already recorded a
            // fatal on the halt path; surface the reason either way.
            Err(err) => Err(err.to_string()),
        }
    }

    /// route a committed peer block's events by mode: HOLD parks the follow-ups
    /// in the oracle queue (drained one-per-step); AUTO drives them to a fixpoint
    /// now (`oracle_queue` is a hold-mode concept — auto never fills it). the
    /// reply already carries the block, so a drive fault is swallowed here (a
    /// fatal already halted; a budget-exceeded is logged inside `drive_auto`).
    async fn settle(&mut self, drained: &[DrainedFrame], events: Vec<Event>) {
        if self.auto {
            let _ = self.drive_auto(events).await;
        } else {
            self.offer(drained, events).await;
        }
    }

    /// route a block's events to the workers (shared try-decode routing) and PARK
    /// the follow-ups in the oracle queue — HOLD-mode drain discipline (each
    /// drains one-per-step). an unclaimed event is a module's only diagnostic
    /// channel (a wasm guest cannot log); a decodable-but-unhandled one means a
    /// saga is stuck Pending.
    async fn offer(&mut self, drained: &[DrainedFrame], events: Vec<Event>) {
        let height = drained
            .iter()
            .map(|d| d.height)
            .max()
            .unwrap_or_else(|| self.height());
        let worker::Offered { follows, unclaimed } = worker::offer(&self.workers, events).await;
        self.oracle_queue.extend(follows);
        let mut notes = noded::log::ModuleNotes::new(height);
        for eff in &unclaimed {
            notes.unclaimed(eff);
        }
        notes.finish();
    }

    /// the current committed height — `node.finalized()` once a block sealed,
    /// else the index watermark the node resumed at (0 on a fresh dir).
    fn height(&self) -> u64 {
        self.node
            .finalized()
            .map(|f| f.height)
            .unwrap_or(self.resume_height)
    }

    /// record a FATAL boundary fault: log, halt (graceful shutdown), and return
    /// the reason. a half-committed host is indeterminate — a SIM that limped
    /// past it would hand tests green runs over corrupt state; as a LIB it cannot
    /// `process::exit`, so it records the reason (`SimHandle::wait` surfaces it,
    /// the binary turns that into exit 1) and tears down.
    fn fatal(&self, err: node::Error) -> String {
        let reason = err.to_string();
        tracing::error!(target: "ducktape::node", error = %reason, "FATAL: halting");
        self.halt(reason.clone());
        reason
    }

    /// the held submitter's reply for a one-op commit: `Ok(BlockSummary)` for an
    /// applied op, `Err(reason)` for a rejected one (the block STILL sealed —
    /// validator parity — but the op moved no state). the sim feeds exactly one
    /// op per non-batch commit, so there is exactly one member frame.
    fn member_summary(drained: &[DrainedFrame]) -> Result<BlockSummary, String> {
        let Some(frame) = drained.iter().find(|d| d.op.is_some()) else {
            return Err("commit produced no member".into());
        };
        let block = BlockSummary {
            height: frame.height,
            root_hash: hex_root(&frame.root_hash),
        };
        match frame.disposition {
            node::Disposition::Applied => Ok(block),
            node::Disposition::Rejected => Err(frame.reason.clone().unwrap_or_default()),
            // a discarded frame carries no `reason`, so the rejected arm would
            // hand the submitter an EMPTY error. it cannot happen in the sim
            // (no cutover ceiling), and the arm exists so a fourth variant
            // fails the build rather than inheriting that empty string.
            node::Disposition::Discarded => Err("discarded at the cutover ceiling".into()),
        }
    }

    /// the `CommittedInfo` for an APPLIED one-op commit; `None` if the op was
    /// rejected (the step reply reports no commit, the submitter got the
    /// rejection). `op_hash` re-stages the payload (idempotent, content-address).
    fn committed_info(
        &self,
        drained: &[DrainedFrame],
        kind: &'static str,
    ) -> Option<CommittedInfo> {
        let frame = drained.iter().find(|d| d.op.is_some())?;
        if frame.disposition != node::Disposition::Applied {
            return None;
        }
        let op = frame.op.as_ref()?;
        Some(CommittedInfo {
            height: frame.height,
            root_hash: hex_root(&frame.root_hash),
            op_hash: hex_bytes(&self.blobs.put_chunk(op.payload.clone())),
            target: op.target.clone(),
            kind,
        })
    }

    /// publish the current committed snapshot into the shared `/v1/status`
    /// cell — the http route reads the cell, never the command lane. the
    /// peers standing rides along (no mesh: height only, no roles or epoch).
    fn publish_status(&self) {
        let cell = self.handle.status_cell();
        cell.publish(self.status());
        cell.publish_peers(noded::PeersStanding {
            height: self.height(),
            ..Default::default()
        });
    }

    fn status(&self) -> NodeStatus {
        let host = self.node.host();
        // the host's live set, sorted by id: the genesis selection plus every
        // module the registry admitted since.
        let modules = host
            .module_roots()
            .into_iter()
            .map(|(id, root)| ModuleStatus {
                category: ModuleCategory::of(&id),
                root: hex_root(&root),
                id,
            })
            .collect();
        let height = self.height();
        NodeStatus {
            contract: noded::NODE_CONTRACT,
            version: env!("CARGO_PKG_VERSION").into(),
            root_hash: hex_root(&host.root_hash()),
            height,
            consensus_time: self.node.stamp_consensus_time(height),
            consensus_time_unit: self.node.consensus_time_policy().into(),
            modules,
            // empty unless `--node-key` fabricated one: clients treat an empty
            // key as "no peer-routed features here" (no huddle voice). the
            // seeded key names an identity for consensus-op scenarios; no mesh
            // routes behind it.
            public_key: self.public_key.clone(),
            chain_id: LOCAL_CHAIN_ID.into(),
            operations: noded::OperationalStatus {
                role: noded::NodeRole::Local,
                phase: noded::NodePhase::Serving,
                ..Default::default()
            },
        }
    }

    fn snapshot(&self) -> SimSnapshot {
        SimSnapshot {
            height: self.height(),
            held: self.held.len(),
            oracle_queued: self.oracle_queue.len(),
            auto: self.auto,
            persona: *self.persona.lock().expect("persona poisoned"),
        }
    }

    /// record a fatal reason and request graceful shutdown — the embeddable
    /// replacement for `process::exit(1)`. only the FIRST reason sticks (a
    /// half-committed host may fault repeatedly as it tears down). the serve
    /// loop drains on `shutdown_requested`; the actor exits when its control
    /// channel closes (the embedder's teardown drops the last sender).
    fn halt(&self, reason: String) {
        let mut fatal = self.fatal.lock().expect("fatal flag poisoned");
        if fatal.is_none() {
            *fatal = Some(reason);
        }
        self.handle.request_shutdown();
    }
}

/// the AUTO-mode reactor lane: each worker follow-up commits as its own block on
/// the ordered lane (via [`Sim::commit_block`]), returning that block's events
/// for the next round; `pending` reports the committed dispatch mailbox so
/// [`worker::drive`] nudges a stranded delivery. `Sim::workers` is moved out
/// while this borrows `&mut Sim`.
struct AutoLane<'a> {
    sim: &'a mut Sim,
}

#[async_trait::async_trait(?Send)]
impl worker::Lane for AutoLane<'_> {
    async fn submit(&mut self, follow: Msg) -> Result<Vec<Event>, worker::Error> {
        match self
            .sim
            .commit_block(vec![(Origin::External(ORACLE_ORIGIN.to_vec()), follow)])
            .await
        {
            Ok((drained, events)) => {
                // a rejected follow-up journaled its block (validator parity) but
                // moved no state; log it and feed no events onward.
                let rejected = drained
                    .iter()
                    .any(|d| d.disposition != node::Disposition::Applied);
                if rejected {
                    tracing::warn!(
                        target: "ducktape::modules",
                        "worker follow-up REJECTED — the oracle's result never landed"
                    );
                    return Ok(Vec::new());
                }
                Ok(events)
            }
            // commit_block already halted on this fatal; break the drive loop.
            Err(reason) => Err(worker::Error::Worker(reason)),
        }
    }

    async fn pending(&self) -> bool {
        match self.sim.node.host().has_pending_work().await {
            Ok(pending) => pending,
            // an unreadable queue fails the next block closed on its own; the
            // pump does not manufacture one.
            Err(e) => {
                tracing::warn!(
                    target: "ducktape::modules",
                    error = %e,
                    reason = "pending_work_unreadable",
                    "could not read the committed queues"
                );
                false
            }
        }
    }
}

/// the disposition of a drained frame as its explorer wire twin, or `None` for
/// a frame that owns no explorer row. `Discarded` is that `None`: it never ran
/// and is re-proposed under the same id at cutover, so publishing it as
/// `Rejected` would lie in the `/v1/blocks` twin the sim exists to make
/// trustworthy — both sibling mappers drop it the same way
/// (`noded::projection::project_block` skips the member, `bin/node`'s
/// sealed-frame row drops the block). it cannot reach the sim today
/// (`StepOrderer` sets no cutover ceiling); every arm is named anyway, so a
/// fourth variant fails the build here as it already does in the siblings.
fn block_disposition(disposition: node::Disposition) -> Option<BlockDisposition> {
    match disposition {
        node::Disposition::Applied => Some(BlockDisposition::Applied),
        node::Disposition::Rejected => Some(BlockDisposition::Rejected),
        node::Disposition::Discarded => None,
    }
}

/// resolve a submit/peer-block origin string's raw bytes: an origin prefixed
/// `hex:` decodes the (any even-length) hex remainder to raw bytes — the only
/// way a JSON-string origin lane can name a real 32-byte ed25519 key. malformed
/// hex is an error, never a silent fall-through to the literal `hex:…` bytes.
/// any other origin passes through verbatim (the trusted-client string convention).
fn decode_origin(origin: Vec<u8>) -> Result<Vec<u8>, String> {
    match origin.strip_prefix(b"hex:") {
        Some(rest) => {
            let hex = std::str::from_utf8(rest)
                .map_err(|_| "hex: origin escape is not valid utf-8".to_string())?;
            duckfs_core::unhex(hex).map_err(|e| format!("hex: origin escape: {e}"))
        }
        None => Ok(origin),
    }
}

fn proposer_hex(origin: &Origin) -> String {
    match origin {
        Origin::External(key) => hex_bytes(key),
        Origin::Module(id) => format!("module:{id}"),
        Origin::Program(account) => format!("acct:{account}"),
        Origin::System => "system".into(),
    }
}

/// noded's debug echo oracle, unconditional here: the sim is a dev tool, and a
/// deterministic canned reply is the ONLY oracle that belongs in it.
struct EchoWorker;

#[async_trait::async_trait(?Send)]
impl worker::Worker for EchoWorker {
    async fn run(&self, event: &Event) -> Result<worker::WorkOutcome, worker::Error> {
        let Ok(request) = saga::decode_worker_request(&event.payload) else {
            return Ok(worker::WorkOutcome::NotMine);
        };
        // a dispatch-plane WorkSpec echoes its raw-text lane (the dispatch
        // module judged a Text contract; the agent module normalizes).
        let Ok(work) = dispatch::decode_work_spec(&request.spec) else {
            return Ok(worker::WorkOutcome::NotMine);
        };
        let saga::WorkerRequest {
            saga_id,
            attempt,
            assignee,
            ..
        } = request;
        // the saga guest runs the STRICT lease policy, so this walks the same
        // bid-then-work lane a real compute node does: an UNASSIGNED request is
        // an announcement, claimed with `Accept` (the module then re-emits the
        // work order naming the winner), and only the assignee's own result
        // lands. the sim commits every follow-up under `ORACLE_ORIGIN`, so that
        // is the key this worker holds its leases under.
        let follow = match assignee.as_deref() {
            None => saga::SagaMsg::Accept { saga_id, attempt },
            Some(ORACLE_ORIGIN) => saga::SagaMsg::OracleResult {
                saga_id,
                attempt,
                outcome: Ok(format!("echo: handling dispatch {}", work.dispatch_id).into_bytes()),
                usage: None,
            },
            // an order another node won is not this worker's to answer.
            Some(_) => return Ok(worker::WorkOutcome::NotMine),
        };
        Ok(worker::WorkOutcome::Handled(Some(Msg {
            target: "saga".into(),
            payload: saga::encode_msg(&follow),
        })))
    }
}

// ── The /sim control routes ─────────────────────────────

fn sim_router(handle: ControlState) -> Router {
    Router::new()
        .route("/sim/step", post(sim_step))
        .route("/sim/auto", post(sim_auto))
        .route("/sim/persona", post(sim_persona))
        .route("/sim/peer-block", post(sim_peer_block))
        .route("/sim/state", get(sim_state))
        .with_state(handle)
}

/// send one control command and await its reply; a closed actor is a 503
/// (the sim is shutting down), matching how the /v1 lane degrades.
async fn control<T, F>(handle: ControlState, build: F) -> Result<T, Response>
where
    F: FnOnce(oneshot::Sender<T>) -> SimCommand,
{
    let (reply_tx, reply_rx) = oneshot::channel();
    let mut sender = handle.control;
    if sender.try_send(build(reply_tx)).is_err() {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    reply_rx
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())
}

async fn sim_step(State(handle): State<ControlState>) -> Response {
    match control(handle, |reply| SimCommand::Step { reply }).await {
        Ok(report) => Json(report).into_response(),
        Err(resp) => resp,
    }
}

async fn sim_auto(State(handle): State<ControlState>, Json(req): Json<AutoRequest>) -> Response {
    match control(handle, |reply| SimCommand::SetAuto {
        enabled: req.enabled,
        reply,
    })
    .await
    {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(resp) => resp,
    }
}

async fn sim_persona(
    State(handle): State<ControlState>,
    Json(req): Json<PersonaRequest>,
) -> Response {
    match control(handle, |reply| SimCommand::SetPersona {
        persona: req.persona,
        reply,
    })
    .await
    {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(resp) => resp,
    }
}

/// encode one peer op's wire fields for the actor: json payload → bytes, and the
/// origin string → bytes (the `hex:` escape is resolved later, in the actor).
/// the default author is `peer`, the concurrent-writer convention. the error is
/// the raw `(status, message)` (small — the caller turns it into a Response).
fn encode_peer_op(op: PeerOp) -> Result<PeerOpWire, (StatusCode, String)> {
    let payload = serde_json::to_vec(&op.payload)
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let origin = op
        .origin
        .map(String::into_bytes)
        .unwrap_or_else(|| PEER_ORIGIN.to_vec());
    Ok((op.target, payload, origin))
}

async fn sim_peer_block(
    State(handle): State<ControlState>,
    Json(req): Json<PeerBlockRequest>,
) -> Response {
    // N members committed as ONE block via submit_block.
    let mut encoded = Vec::with_capacity(req.ops.len());
    for op in req.ops {
        match encode_peer_op(op) {
            Ok(parts) => encoded.push(parts),
            Err((status, msg)) => return (status, msg).into_response(),
        }
    }
    match control(handle, |reply| SimCommand::PeerBlock {
        ops: encoded,
        reply,
    })
    .await
    {
        Ok(Ok(info)) => Json(info).into_response(),
        Ok(Err(rejection)) => (StatusCode::BAD_REQUEST, rejection).into_response(),
        Err(resp) => resp,
    }
}

async fn sim_state(State(handle): State<ControlState>) -> Response {
    match control(handle, |reply| SimCommand::Snapshot { reply }).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(resp) => resp,
    }
}

// ── Networked-persona receipt shaping ───────────────────

/// the networked validator's submit reply is height-only — `op_hash` is a
/// local-daemon receipt field. noded's shared submit handler always adds it,
/// so the networked persona strips it at the response layer instead of
/// forking the handler.
async fn strip_receipt_op_hash(
    State(persona): State<Arc<Mutex<Persona>>>,
    req: Request,
    next: Next,
) -> Response {
    let is_submit = req.uri().path() == "/v1/submit";
    let resp = next.run(req).await;
    if !is_submit
        || resp.status() != StatusCode::OK
        || *persona.lock().expect("persona poisoned") != Persona::Networked
    {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = match axum::body::to_bytes(body, RECEIPT_BODY_CAP).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let stripped = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|mut value| {
            value.as_object_mut()?.remove("op_hash");
            serde_json::to_vec(&value).ok()
        });
    // the buffered body replaces the streamed one, so the recorded length is
    // stale either way — drop it and let hyper size the new body.
    parts.headers.remove(header::CONTENT_LENGTH);
    match stripped {
        Some(new_body) => Response::from_parts(parts, Body::from(new_body)),
        // not a json object (unexpected) — pass the original bytes through.
        None => Response::from_parts(parts, Body::from(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// the sim's `/v1/blocks` twin must map every disposition the way the two
    /// production mappers do — applied and rejected publish a member row, and a
    /// discarded frame publishes NONE (it never ran). the arm this pins used to
    /// be a `_ =>` that read `Discarded` as `Rejected`.
    #[test]
    fn a_discarded_frame_publishes_no_member_row() {
        assert_eq!(
            block_disposition(node::Disposition::Applied),
            Some(BlockDisposition::Applied)
        );
        assert_eq!(
            block_disposition(node::Disposition::Rejected),
            Some(BlockDisposition::Rejected)
        );
        assert_eq!(block_disposition(node::Disposition::Discarded), None);
    }

    /// the sim binary and a real node are run side by side all day, so their
    /// eagerly-bound defaults must not overlap. `workspace-config` owns the
    /// node's operator block — HTTP 8844, admin RPC 8845, mesh 8846 — and this
    /// crate deliberately does not link it, so the block is restated here as
    /// the thing [`DEFAULT_LISTEN`] must stay clear of. A default back inside
    /// it means the second process to boot dies on its bind, and a client on
    /// that port reaches whichever daemon won, speaking the wrong protocol.
    #[test]
    fn the_default_listen_avoids_the_nodes_operator_ports() {
        let node_operator_ports = 8844..=8846;
        let addr: SocketAddr = DEFAULT_LISTEN.parse().expect("DEFAULT_LISTEN parses");
        let clear_of_the_node = !node_operator_ports.contains(&addr.port());
        assert!(
            clear_of_the_node,
            "the sim default {addr} is inside the node's operator block \
             {node_operator_ports:?} — it will fight a real node for the bind"
        );
    }

    /// WHERE the status snapshot is published is load-bearing, and the bug it
    /// replaced was a race — so this pins the SHAPE, which is deterministic,
    /// instead of trying to lose the race on purpose.
    ///
    /// The contract: a caller who reads `/v1/status` the instant its receipt for
    /// height N arrives must be told N, never N-1. That holds only while the
    /// publish happens INSIDE `commit_block` — before the committing arm sends
    /// its reply. It used to fire once per pump turn, i.e. AFTER the reply had
    /// already gone out; the symptom was a ~1-in-15 red in
    /// `governance_scenarios.rs` asserting the tip equals its own receipt, which
    /// is far too rare to be the guard for this.
    ///
    /// So: exactly two call sites, both named. Boot (the snapshot the listener
    /// waits on) and the commit funnel. A third — especially one back in the
    /// pump loop — means the ordering regressed.
    #[test]
    fn the_status_snapshot_is_published_from_boot_and_the_commit_funnel_alone() {
        // the production half only — below `#[cfg(test)]` this very test quotes
        // the call it is looking for, and would count itself.
        let (source, _) = include_str!("lib.rs")
            .split_once("#[cfg(test)]")
            .expect("the test module marks the end of the production source");
        assert_eq!(
            source.matches("self.publish_status();").count(),
            1,
            "expected exactly ONE `self.publish_status()` — the commit funnel's. \
             A second (notably one per pump turn) lands AFTER the committing arm \
             already replied, which is the race this shape exists to make \
             unobservable."
        );
        // and it must be the commit funnel's: the statement immediately
        // preceding `commit_block`'s return.
        let (before_return, _) = source
            .split_once("        Ok((drained, self.node.take_events()))")
            .expect("commit_block's return is the anchor for this check");
        assert!(
            before_return.trim_end().ends_with("self.publish_status();"),
            "`commit_block` must publish the snapshot as its last act before \
             returning — that is what makes the publish precede every caller's \
             reply."
        );
    }

    /// once a fatal reason is recorded, the embedded control surface fails
    /// closed — every method routes through `call`, so one guard covers all.
    /// we set the flag DIRECTLY rather than provoke a real `SubmitError::Fatal`
    /// (there is no cheap way to force host corruption); the guard is the unit
    /// under test. `wait`'s path is unchanged and covered by the binary suite.
    #[test]
    fn fatal_flag_fails_the_embedded_control_surface_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = boot(
            dir.path(),
            "127.0.0.1:0".parse().expect("addr"),
            SimOpts::default(),
        )
        .expect("boot");
        assert!(handle.state().is_ok(), "healthy before the flag is set");

        *handle.fatal.lock().expect("fatal") = Some("boom".into());
        assert_eq!(handle.step().unwrap_err(), "boom");
        assert_eq!(handle.state().unwrap_err(), "boom");
        assert_eq!(handle.set_auto(false).unwrap_err(), "boom");
        assert_eq!(
            handle
                .peer_block(serde_json::json!({ "ops": [{ "target": "chat", "payload": {} }] }))
                .unwrap_err(),
            "boom",
        );
    }
}
