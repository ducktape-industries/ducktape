//! the ducktape node daemon: ONE embedded host behind http/ws.
//!
//! two runtimes, one thread each way: the node actor owns the (non-Send)
//! `host::Host` inside a commonware `tokio::Runner` — the qmdb-backed modules
//! need its storage context — and drains [`NodeCommand`]s in arrival order, one
//! msg per block. the axum server runs on a plain tokio runtime on the main
//! thread and only ever talks to the actor over the command channel. every app
//! build is a client: the web build dials this directly; the desktop shell
//! spawns it detached (an orphan — it outlives the window) and connects the
//! same way. POST /v1/admin/shutdown is how a client retires it: no pid handshake,
//! the port IS the daemon's identity.
//!
//! run: `cargo run -p noded-bin -- [--listen 127.0.0.1:8844] [--storage <dir>]
//! [--modules <dir>]`
//!
//! without `--storage` state lives in a fresh temp dir (clean run each boot).
//! with it, qmdb modules, the forge repo, and the per-module index persist;
//! the local block counter resumes ABOVE the index's watermark, so op-log
//! heights stay monotonic across restarts (a counter restarting at 0 would
//! make every new block look already-indexed and be silently skipped).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use commonware_runtime::{Metrics as _, Runner as _, Supervisor as _};
use futures::StreamExt as _;
use futures::channel::mpsc;
use host::{BlockContext, Host, SubmitError};
use indexer::IndexStore;
use noded::bundle::{DirCodeSource, qmdb_stores};
use noded::compose::{Admissions, Bindings, Boot, Substrates, compose};
use noded::{
    BlockDisposition, BlockRecord, BlockSummary, LOCAL_CHAIN_ID, ModuleCategory, ModuleStatus,
    NodeCommand, NodeHandle, NodeMetrics, NodeStatus, StreamHub, block_row, hex_root,
};
use sdk::{Event, Msg, Origin};
use topology::TOPOLOGY;

/// every module registered at genesis — the `sim_base` selection of the
/// single-source [`topology`] (identical to simnode's default set). `run_node` composes exactly these ids through the topology
/// composer; status reports list the host's live set, which grows with the
/// modules registry's admissions.
const MODULE_IDS: &[&str] = topology::SIM_BASE;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut listen: SocketAddr = "127.0.0.1:8844".parse()?;
    let mut storage: Option<PathBuf> = None;
    let mut modules: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().ok_or("--listen needs an addr")?.parse()?,
            "--storage" => storage = args.next().map(PathBuf::from),
            "--modules" => {
                modules = Some(PathBuf::from(args.next().ok_or("--modules needs a dir")?))
            }
            other => {
                return Err(format!(
                    "unexpected arg {other:?} (want --listen/--storage/--modules)"
                )
                .into());
            }
        }
    }
    // where the wasm tenants' `<id>.component.wasm` bytes come from: `--modules`,
    // else the founding set the build staged beside this binary. This daemon
    // runs no network, so it composes straight from that set — a node on a
    // network composes from its workspace genesis instead. Read and hashed
    // HERE, before a storage root is created or a listener binds — ONE
    // decision about whether the set is usable, made where its remedy is the
    // operator's rather than a stack trace from the actor thread. the source
    // (a path and a hash table) then rides into the actor.
    let modules_dir = match modules {
        Some(dir) => dir,
        None => workspace_config::modules_dir()?,
    };
    let wasm_ids = TOPOLOGY.wasm_ids(MODULE_IDS);
    let (code, code_hashes) = DirCodeSource::open(&modules_dir, &wasm_ids).map_err(|err| {
        format!(
            "{err} — `cargo build` stages the founding set beside the binary; or pass \
             --modules <dir> holding every <id>.component.wasm and <id>.index.wasm"
        )
    })?;
    let storage = storage.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("ducktape-noded-{}", std::process::id()))
    });
    // forge's on-disk repo base, derived ONCE: the node actor's `Forge` and the
    // http git upload-pack (clone) lane must agree on it, so both are handed the
    // same path — the actor to materialize into, the http handle to serve from.
    let forge_repo = storage.join("forge-git");

    // the per-module derived index: one fluent31 database per module under
    // <storage>/index/<module>/, with each module's view mapper registered.
    // an open failure is fatal-with-remedy rather than a silent no-index run:
    // the tier is rebuildable, so the fix is always "delete <storage>/index".
    // this daemon runs no network, so its index guests come from the same
    // founding set its components do, not from a genesis.
    let index = noded::open_index_store(&storage, MODULE_IDS)?;

    let log_ring = noded::LogRing::default();
    noded::log::init(Some(log_ring.clone()), Some(storage.join("daemon.log")));

    let (handle, cmd_rx, stream_hub) = NodeHandle::channel_with_log_ring(log_ring);
    let handle = handle
        // persist node-local blobs (op receipts, agent prompt pins) under
        // <storage>/blobstore so a daemon restart keeps serving them.
        .with_blob_root(storage.join("blobstore"))?
        .with_forge_repo(forge_repo.clone())
        .with_index_store(index.clone())
        // the duckfs workspace RPC materializes managed checkouts here (disk
        // state, separate from the module's own `<storage>/duckfs` dir).
        .with_duckfs_workspaces(storage.join("duckfs-workspaces"))
        // the single-writer daemon has no consensus and no on-chain owner, so
        // admin is operator-gated: the credential minted into
        // <storage>/admin.token 0600 is what a client presents — and what the
        // mutating `/v1` write gate takes from a local daemon that has no user
        // key. `DUCKTAPE_ADMIN=off` removes the control surface entirely; the
        // credential is still minted, because the write gate still wants it.
        .with_admin(noded::AdminConfig::minted(
            noded::AdminExposure::from_env(),
            &storage,
        ));

    // the node actor gets its own thread: commonware's tokio runner owns that
    // thread's runtime, and the host must never leave it. the blob handle is
    // the node-local surface that crosses: the actor registers the files module
    // over the blobs, while the http layer reads through its own clone.
    let actor_storage = storage.clone();
    let actor_forge_repo = forge_repo.clone();
    let actor_index = index.clone();
    let blobs = handle.blob_handle();
    // the readiness event: the actor publishes its FIRST status snapshot
    // (genesis or the resumed height) and signals; the http listener is not
    // bound until then. `/v1/status` answering 200 is what every client uses as
    // "this daemon is up" — the desktop spawn probe, the CLI, the e2e harness —
    // so serving before the actor's boot publish hands them a snapshot claiming
    // version "", root_hash "", no modules and height 0. On a restart over
    // existing storage that last one is an outright lie about committed state.
    let (booted_tx, booted_rx) = std::sync::mpsc::sync_channel::<()>(1);
    // a full handle clone for the actor loop's own surfaces (status, blobs,
    // the stream hub). The agent-run provisioner is NOT here any more: it lives
    // in the compute daemon and reaches this node over /v1.
    let actor_handle = handle.clone();

    // the node-local, off-chain interactive terminal-session plane (lives in the
    // daemon like the stream hub — never consensus). This node spawns no pty:
    // the plane is the RINGS and the metadata, and an agent daemon (`ducktape
    // service run agent`) attaches over the ws to own the ptys. With none
    // attached, create returns a clear 503 — a bare spawn is unrepresentable.
    let term_ring = handle.stream_hub().terminals();
    let term_cmd_ring = handle.stream_hub().term_commands();
    // no link token: this test daemon has no workspace to hold one, so it
    // refuses every attach and therefore has no interactive plane. Nothing
    // runs an agent daemon against it.
    let handle =
        handle.with_terminals(noded::TerminalSessions::new(term_ring, term_cmd_ring, None));
    std::thread::Builder::new()
        .name("node-actor".into())
        .spawn(move || {
            run_node(
                actor_storage,
                actor_forge_repo,
                code,
                code_hashes,
                actor_index,
                blobs,
                actor_handle,
                cmd_rx,
                stream_hub,
                booted_tx,
            )
        })?;

    // a dropped sender means the actor thread died inside genesis — report that
    // instead of blocking forever on a boot that will never happen.
    booted_rx
        .recv()
        .map_err(|_| "node actor died during genesis — see the error above")?;

    tracing::info!(
        target: "ducktape::node",
        %listen,
        storage = %storage.display(),
        "noded listening"
    );
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let listener = tokio::net::TcpListener::bind(listen).await?;
            noded::serve(listener, handle).await?;
            // in-flight requests drained; blocks commit at the block boundary,
            // so exiting here loses nothing.
            tracing::info!(target: "ducktape::node", "noded shutdown requested; exiting");
            Ok(())
        })
}

/// own the host for the process lifetime: genesis the module set, then apply
/// commands in arrival order — every submit is its own block.
// the actor thread's entry point threads every daemon-owned root/lane in by
// value (storage, forge, index, blobs, the actor handle, the command receiver,
// the event fan-out); bundling them into a struct would only rename the list.
#[allow(clippy::too_many_arguments)]
fn run_node(
    storage: PathBuf,
    forge_repo: PathBuf,
    code: DirCodeSource,
    code_hashes: BTreeMap<String, [u8; 32]>,
    index: Arc<IndexStore>,
    blobs: noded::blobs::BlobHandle,
    node_handle: noded::NodeHandle,
    mut cmds: mpsc::Receiver<NodeCommand>,
    stream_hub: StreamHub,
    booted: std::sync::mpsc::SyncSender<()>,
) {
    // forge_repo is derived by the caller (shared with the http upload-pack lane).
    let duckfs_dir = storage.join("duckfs");
    // kept past the runtime config: the retained-store sampler walks it.
    let store_root = storage.clone();
    let rt_cfg = commonware_runtime::tokio::Config::default().with_storage_directory(storage);
    let executor = commonware_runtime::tokio::Runner::new(rt_cfg);

    executor.start(|context| async move {
        // genesis: the topology's `sim_base` selection composed the way bin/node
        // composes — every wasm tenant is the REAL guest component, fetched by
        // hash from the source `main` opened, over qmdb stores in this runtime's
        // storage root.
        // the block loop's own handle: each block's root payload is staged as
        // its explorer row is built, so op hashes stay dereferencable via the
        // blob lane (worker follow-ups included — the http submit handler only
        // stages what clients POST).
        let op_blobs = blobs.clone();
        let substrates = Substrates {
            directory: duckfs_dir.with_file_name("module-storage"),
            bindings: [("forge".into(), forge_repo), ("files".into(), duckfs_dir)].into(),
            blobs,
        };
        let bindings = Bindings {
            // no governance in this set, so nothing reads an invite namespace.
            invite: b"",
            chain_id: LOCAL_CHAIN_ID,
            // the daemon arms no `ConsensusTimePolicy`, so its clock IS the
            // height — the same unit it reports as `consensus_time_unit`.
            time_unit: sdk::genesis_config::TimeUnit::Height,
        };
        let mut stores = qmdb_stores(&context);
        let mut host = compose(
            &code,
            &mut stores,
            &substrates,
            &bindings,
            Boot::Genesis {
                // no valset: the single-writer daemon has no validators.
                validators: &[],
                bundle: &code_hashes,
            },
        )
        .await
        .expect("noded genesis composes");
        host.set_module_factory(Box::new(Admissions::new(&context, &substrates, &bindings)));
        noded::converge_host_modules(&index, &host).expect("deployed index guests converge");

        tracing::info!(
            target: "ducktape::consensus",
            root_hash = %hex_root(&host.root_hash()),
            "noded genesis"
        );

        // register the daemon's `ducktape_*` series on the runtime registry —
        // one `context.encode()` then serves them alongside commonware's own
        // runtime metrics. the handles are retained for the block loop's life.
        let metrics = NodeMetrics::register(&context);
        metrics.set_role_phase(noded::NodeRole::Local, noded::NodePhase::Serving);
        // the retained stores' footprint, on its own slow background task —
        // the blobstore and the derived index keep every applied op payload
        // forever and nothing prunes either (#1309), and the daemon fills
        // both exactly as a validator does.
        noded::spawn_store_footprint_sampler(metrics.clone(), store_root);

        // the observability cell: this single-writer loop is the ONE
        // publisher; the status/peers routes read the cell without crossing
        // the command lane, operations overlay live from the metrics, and
        // /metrics + the ws metrics topic encode the registry through the
        // wired exposition source. no mesh identity here — the peers
        // standing stays role-less, and the sample parses honestly empty.
        let status = node_handle.status_cell();
        status.wire_metrics(&metrics);
        stream_hub.wire_metrics(&metrics);
        // `Context` has no Clone; a child shares the SAME registry (the
        // label only prefixes new registrations), so its encode() serves
        // the identical exposition.
        let exposition_context = context.child("exposition");
        status.wire_exposition(move || exposition_context.encode());

        // NO in-process compute plane: dispatch work is executed by the
        // standalone compute daemon, which reaches this node over its own /v1
        // surface like any other client. The local reactor drains only the
        // committed onchain delivery and program-call queues.
        // resume the local block counter ABOVE the index watermark: the op
        // log persists under --storage, and a counter restarting at 0 would
        // re-use indexed heights — every new block silently skipped.
        let mut height = index.resume_height().expect("read index watermarks");
        stream_hub.prime(height, hex_root(&host.root_hash()));
        if height > 0 {
            tracing::info!(
                target: "ducktape::modules",
                height,
                "noded module index resumed"
            );
        }
        // stamp modules whose watermark trails the resume floor — a wiped (or
        // torn) per-module database that forward folding can never refill,
        // because its heights are already spent above it. its feed and views
        // start over at the boundary, visibly via /v1/index/status; history
        // below it re-enters only by replaying blocks through the feed.
        match noded::stamp_stale_modules(&index, height) {
            Ok(stamped) => {
                for module in stamped {
                    tracing::info!(
                        target: "ducktape::modules",
                        module,
                        height,
                        "noded module index stamped backfilled at the boundary"
                    );
                }
            }
            Err(err) => {
                // poisoned, not fatal: reads stay up, writes refuse, and the
                // wipe-to-rebuild remedy is the same one the fold error names.
                tracing::error!(
                    target: "ducktape::consensus",
                    error = %err,
                    index = %index.base().display(),
                    "module index rebuild FAILED — the app's views are STALE; wipe the \
                     index directory to rebuild"
                );
            }
        }
        // the boot snapshot: resumed (or genesis) state serves immediately —
        // /v1/status answers before the first command, never behind it. the
        // signal is what makes "immediately" true: main binds the listener only
        // after this publish, so the daemon's first answer is never the empty
        // default snapshot.
        publish_status(&status, &metrics, &index, &host, height);
        let _ = booted.send(());
        while let Some(cmd) = cmds.next().await {
            match cmd {
                NodeCommand::Submit {
                    target,
                    payload,
                    origin,
                    reply,
                } => {
                    let result = submit_and_drain(
                        &mut host,
                        &mut height,
                        &index,
                        &op_blobs,
                        &stream_hub,
                        &metrics,
                        Origin::External(origin),
                        Msg { target, payload },
                    )
                    .await;
                    publish_status(&status, &metrics, &index, &host, height);
                    let _ = reply.send(result); // caller may have hung up
                }
                NodeCommand::SubmitFrame { frame, reply } => {
                    // the frame lane is FAITHFUL to the real node here, and that
                    // is the whole point of it: the origin is the frame's
                    // VERIFIED signer, never the caller's claim. the frameless
                    // arm above trusts a client string — a convention a local
                    // daemon can afford — and `bin/node` discards that string
                    // outright, so an embedded daemon that also stamped a claimed
                    // origin HERE would let an e2e pass on attribution production
                    // would never produce. it decodes exactly as the validator's
                    // ordered drain does instead.
                    let result = match node::decode_frame(&frame) {
                        Ok((origin, msg)) => {
                            submit_and_drain(
                                &mut host,
                                &mut height,
                                &index,
                                &op_blobs,
                                &stream_hub,
                                &metrics,
                                origin,
                                msg,
                            )
                            .await
                        }
                        // junk never reaches the store: the http gate already
                        // refused it, and this is the second wall for any
                        // embedder-side producer on the command lane.
                        Err(err) => Err(err.to_string()),
                    };
                    publish_status(&status, &metrics, &index, &host, height);
                    let _ = reply.send(result);
                }
                NodeCommand::Query { target, req, reply } => {
                    let result = host
                        .query(&target, &req)
                        .await
                        .map_err(|err| err.to_string());
                    let _ = reply.send(result);
                }
                NodeCommand::QueryAs {
                    target,
                    req,
                    reader,
                    reply,
                } => {
                    let result = host
                        .query_as(&target, &req, sdk::Origin::External(reader))
                        .await
                        .map_err(|err| err.to_string());
                    let _ = reply.send(result);
                }
            }
        }
    });
}

/// wall-clock millis for `consensus_time`. a single-writer local block counter
/// has no consensus clock to agree on; wall time keeps module timestamps
/// (message sent_at, task created_at) meaningful to the ui.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is past the epoch")
        .as_millis() as u64
}

/// commit the caller's op, then drain committed onchain work (each its own block).
/// the returned summary is the block that INCLUDED the caller's op — follow-up
/// blocks reach clients over the ws stream, not this reply.
#[allow(clippy::too_many_arguments)]
async fn submit_and_drain(
    host: &mut Host,
    height: &mut u64,
    index: &IndexStore,
    blobs: &noded::blobs::BlobHandle,
    stream_hub: &StreamHub,
    metrics: &NodeMetrics,
    origin: Origin,
    msg: Msg,
) -> Result<BlockSummary, String> {
    let (included, events) =
        match submit_one(host, height, index, blobs, stream_hub, metrics, origin, msg).await {
            Ok(out) => out,
            Err(SubmitError::Fatal(err)) => {
                tracing::error!(target: "ducktape::node", error = %err, "FATAL: halting");
                std::process::exit(1);
            }
            Err(err @ SubmitError::Rejected(_)) => return Err(err.to_string()),
        };

    // The reactor nudges committed delivery and call queues through this
    // lane's block boundary, bounding programs that retrigger themselves.
    let mut lane = PendingLane {
        host: &mut *host,
        height: &mut *height,
        index,
        blobs,
        stream_hub,
        metrics,
    };
    let unclaimed = match host::worker::drive(&[], events, &mut lane).await {
        Ok(unclaimed) => unclaimed,
        Err(host::worker::Error::Fatal(err)) => {
            tracing::error!(target: "ducktape::node", error = %err, "FATAL: halting");
            std::process::exit(1);
        }
        Err(err) => return Err(err.to_string()),
    };

    // an unclaimed event is a module's ONLY diagnostic channel (a wasm guest
    // cannot log) — unless it decodes as a worker request, which means a saga
    // is stuck Pending.
    let mut notes = noded::log::ModuleNotes::new(*height);
    for eff in &unclaimed {
        notes.unclaimed(eff);
    }
    notes.finish();

    Ok(included)
}

/// The local reactor can only nudge committed onchain queues. It has no
/// external workers or provider identity; each nudge commits as a system op.
struct PendingLane<'a> {
    host: &'a mut Host,
    height: &'a mut u64,
    index: &'a IndexStore,
    blobs: &'a noded::blobs::BlobHandle,
    stream_hub: &'a StreamHub,
    metrics: &'a NodeMetrics,
}

#[async_trait::async_trait(?Send)]
impl host::worker::Lane for PendingLane<'_> {
    async fn submit(&mut self, follow: Msg) -> Result<Vec<Event>, host::worker::Error> {
        match submit_one(
            self.host,
            self.height,
            self.index,
            self.blobs,
            self.stream_hub,
            self.metrics,
            Origin::System,
            follow,
        )
        .await
        {
            Ok((_block, events)) => Ok(events),
            Err(SubmitError::Fatal(err)) => Err(host::worker::Error::Fatal(err)),
            Err(err @ SubmitError::Rejected(_)) => {
                tracing::warn!(
                    target: "ducktape::modules",
                    error = %err,
                    "pending-work nudge rejected"
                );
                Ok(Vec::new())
            }
        }
    }

    async fn pending(&self) -> bool {
        match self.host.has_pending_work().await {
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

/// assemble and publish the `/v1/status` snapshot for this single-writer
/// lane: at boot, then after every command that can move state. the storage
/// section rides along so index watermarks stay current with the boundary.
fn publish_status(
    status: &noded::StatusCell,
    metrics: &NodeMetrics,
    index: &IndexStore,
    host: &Host,
    height: u64,
) {
    metrics.update_storage(
        0,
        index.is_poisoned(),
        index.module_ids().into_iter().map(|id| {
            let height = index.applied_height(&id).unwrap_or_default();
            (id, height)
        }),
    );
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
    status.publish(NodeStatus {
        contract: noded::NODE_CONTRACT,
        version: env!("CARGO_PKG_VERSION").into(),
        root_hash: hex_root(&host.root_hash()),
        height,
        // the embedded daemon never arms a `ConsensusTimePolicy` either —
        // height-is-time, same as the validator/replica lanes.
        consensus_time: height,
        consensus_time_unit: noded::ConsensusTimeUnit::Height,
        modules,
        // the embedded daemon has no mesh identity — clients treat an empty
        // key as "no peer-routed features here".
        public_key: String::new(),
        // the chain the composer bound into the identity and gateway guests —
        // the value a client's add-key consent must sign over.
        chain_id: LOCAL_CHAIN_ID.into(),
        operations: metrics.operational_status(),
    });
    // no mesh, no consensus: the standing carries only the height — the
    // peers route parses an honestly empty sample with no roles or epoch.
    status.publish_peers(noded::PeersStanding {
        height,
        ..Default::default()
    });
}

#[allow(clippy::too_many_arguments)]
async fn submit_one(
    host: &mut Host,
    height: &mut u64,
    index: &IndexStore,
    blobs: &noded::blobs::BlobHandle,
    stream_hub: &StreamHub,
    metrics: &NodeMetrics,
    origin: Origin,
    msg: Msg,
) -> Result<(BlockSummary, Vec<Event>), SubmitError> {
    let consensus_time = unix_millis();
    // the row's coordinates, captured before ctx/msg consume them. this lane
    // frames and signs nothing, so the shared projection renders the SUBMITTER's
    // origin as the row `proposer` (hex like the networked lane's keys — identity
    // maps bound node keys to account display names).
    let target = msg.target.clone();
    let payload = msg.payload.clone();
    let ctx = BlockContext {
        height: *height + 1,
        consensus_time,
        origin: origin.clone(),
    };
    // node-local wall-clock cost of applying this block — the metrics plane's
    // one non-deterministic signal (the apply-latency histogram). measured HERE
    // in the effectful daemon layer, never inside the deterministic host, so
    // the kernel stays clock-free.
    let started = Instant::now();
    let out = host.submit_at(ctx, msg).await?;
    let latency_us = started.elapsed().as_micros() as u64;
    *height += 1;

    let block = BlockSummary {
        height: *height,
        root_hash: hex_root(&out.root_hash),
    };
    // fold this block into the Prometheus series (before `out` is consumed).
    metrics.record_block(*height, latency_us, &out.dispatches);
    metrics.record_op_outcomes(1, 0); // this lane is one applied member op per block

    // fold the block into the derived per-module index LAST: canonical state
    // is already committed, so an index failure degrades the read models and
    // never the block. the explorer row via the shared projection seam — RootOp
    // assembly, dispatch trace, payload preview, and op-hash staging (put_chunk
    // keys the blob by sha256, keeping it dereferencable via
    // GET /v1/files/blob/{op_hash}) in ONE shape with the validator lane. every
    // block on this lane is applied (a rejected submit never increments the
    // height, so it never was a block); the frame hash stays empty — nothing is
    // framed here, and an invented digest would claim a verification that never
    // happened.
    let record = Some(block_row(&BlockRecord {
        height: *height,
        hash: String::new(),
        commit_hash: hex_root(&out.root_hash),
        // the embedded daemon lane is 1-op-1-block (one host.submit per block),
        // so the block carries exactly one member op.
        ops: vec![noded::projection::project_root_op(
            blobs,
            &origin,
            &target,
            &payload,
            &out.dispatches,
            BlockDisposition::Applied,
        )],
    }));
    // the shared index-fold epilogue (store poisons itself on error, staying
    // loud on every later block until rebuilt).
    noded::projection::apply_block_to_index(
        index,
        *height,
        consensus_time,
        record,
        &out.dispatches,
        host,
    );

    // fan the block out live after the derived index had its chance to
    // materialize rows. no subscribers is fine.
    stream_hub.publish_block(
        block.height,
        block.root_hash.clone(),
        noded::BlockWake::from_dispatches(&out.dispatches),
    );

    Ok((block, out.events))
}
