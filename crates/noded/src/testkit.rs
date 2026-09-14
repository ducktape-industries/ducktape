//! An in-process node for e2e harnesses: a REAL [`host::Host`] on a dedicated
//! actor thread (the host is deliberately non-Send), fronted by [`crate::serve`]
//! on a loopback listener — the exact shape `bin/noded/src/main.rs`'s daemon
//! runs, minus the oracle/index/metrics machinery a focused test node has no
//! use for. A harness hands it a genesis closure and the module ids to surface
//! in `/v1/status`; the CLI/tool subprocess under test drives it over http like
//! any real daemon.
//!
//! This is the ONE definition of the minimal `NodeCommand` actor. The fs/mcp
//! CLI harnesses each used to hand-mirror this arm-for-arm (their own comments
//! admitted it), which drifts from the daemon it claims to stand in for. Now
//! they build a host and call [`InProcDaemon::start`].
//!
//! Gated behind the `testkit` feature so it never compiles into a shipping
//! node — consumers enable it as a dev-dependency feature only.

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt as _, StreamExt as _};
use host::{BlockContext, Host};
use sdk::{Msg, Origin};

use crate::{
    BlockSummary, ModuleCategory, ModuleStatus, NodeCommand, NodeHandle, NodeStatus, hex_root,
};

/// a running in-process node: an actor thread owning the host and a server
/// thread serving the client surface, torn down cleanly on drop.
pub struct InProcDaemon {
    port: u16,
    /// this harness's admin credential. The harness OWNS the node in-process, so
    /// it is the operator — there is no workspace to write the token into, and
    /// nothing outside this struct ever learns it.
    operator_token: String,
    server: Option<JoinHandle<()>>,
    actor: Option<JoinHandle<()>>,
    drain: mpsc::UnboundedSender<oneshot::Sender<Result<BlockSummary, String>>>,
    /// a spare of the actor's command lane, for [`InProcDaemon::commands`].
    /// Held rather than minted from a handle clone because the handle itself
    /// must not outlive `serve`. `Option` for the same reason `actor` is: it is
    /// DROPPED FIRST in [`Drop`], because a live sender keeps the actor's drain
    /// loop open and the join below would never return.
    commands: Option<mpsc::Sender<NodeCommand>>,
    status: crate::StatusCell,
}

impl InProcDaemon {
    /// stand the node up: build the host on the actor thread via `build_host`
    /// (genesis holds non-Send qmdb handles, so it runs THERE, not on the
    /// caller), serve the router on the server thread, and block until
    /// `/v1/status` answers. `status_modules` are the ids reported in the
    /// status projection (each module's root read straight from the host).
    pub fn start(
        build_host: impl FnOnce() -> Host + Send + 'static,
        status_modules: Vec<String>,
    ) -> Self {
        Self::start_with_blob_root(build_host, status_modules, None)
    }

    /// [`Self::start`] with the node-local blob store rooted on disk.
    ///
    /// Needed whenever a test must put bytes where a MODULE will later look for
    /// them. Forge materializes a pushed packfile OUT of the blob store, so a
    /// test that pushes real git objects has to hand the module the same store
    /// the http surface writes to — with the in-memory default they are two
    /// disconnected stores and the objects are simply not there.
    ///
    /// `None` keeps the in-memory default.
    pub fn start_with_blob_root(
        build_host: impl FnOnce() -> Host + Send + 'static,
        status_modules: Vec<String>,
        blob_root: Option<std::path::PathBuf>,
    ) -> Self {
        Self::start_configured(build_host, status_modules, blob_root, None)
    }

    /// Serve the normal signed HTTP mutation lane bound to this node key.
    /// The harness supplies a real Ed25519 public key, just as a consensus
    /// node wires its key into both status and request verification at boot.
    pub fn start_with_node_key(
        build_host: impl FnOnce() -> Host + Send + 'static,
        status_modules: Vec<String>,
        node_key: commonware_cryptography::ed25519::PublicKey,
    ) -> Self {
        Self::start_configured(
            build_host,
            status_modules,
            None,
            Some(node_key.as_ref().to_vec()),
        )
    }

    fn start_configured(
        build_host: impl FnOnce() -> Host + Send + 'static,
        status_modules: Vec<String>,
        blob_root: Option<std::path::PathBuf>,
        node_key: Option<Vec<u8>>,
    ) -> Self {
        // Bind FIRST, and hand the bound socket to the server thread.
        //
        // The harness used to reserve a port with `nettest::free_port()`, drop
        // the probe, and bind minutes of genesis later — the classic gap. Worse,
        // the bind then lived on the server thread where its `.expect` panicked
        // into nothing, so the readiness loop below could not tell "my listener
        // died" from "not up yet" and would happily settle for a STRANGER's 200
        // on that port. Binding here closes both: there is no gap to lose the
        // port in, a failure to bind panics in the caller's own thread, and the
        // port we report is by construction the one WE hold.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind harness listener");
        let port = listener.local_addr().expect("harness listen addr").port();
        listener
            .set_nonblocking(true)
            .expect("harness listener is nonblocking for tokio");
        let (handle, cmd_rx, hub) = NodeHandle::channel();
        let (drain, drain_rx) = mpsc::unbounded();
        // chained right after `channel()`, before any `blob_handle()` clone is
        // handed out — the ordering `with_blob_root` documents.
        let handle = match blob_root {
            Some(root) => handle.with_blob_root(root).expect("harness blob root"),
            None => handle,
        };
        let status = handle.status_cell();
        // taken here, off the ONE handle: `serve` consumes it below and Drop
        // depends on that being the last one alive.
        let commands = handle.command_sender();
        // the cell the actor thread publishes into, kept for the harness's own
        // readers — the actor's copy moves onto its thread.
        let published = status.clone();
        status.publish(NodeStatus {
            public_key: node_key
                .as_deref()
                .map(crate::hex_bytes)
                .unwrap_or_default(),
            ..Default::default()
        });
        // the testkit has no mesh and no registry: an empty exposition
        // parses to the honest empty peers sample and an empty scrape.
        status.wire_exposition(String::new);
        // the admin namespace is operator-gated, and this harness serves on a
        // REAL loopback port — so it mints a real per-instance credential rather
        // than a shared literal any other local process could guess.
        let operator_token = crate::services::new_secret();
        let handle = handle.with_admin(crate::AdminConfig {
            operator_token: Some(operator_token.clone()),
            node_key,
            ..Default::default()
        });

        // the readiness event, same contract as `bin/noded`'s daemon: genesis
        // runs on the actor thread and publishes the boot snapshot, and only
        // THEN does anything SERVE. The listener is bound above (that is a
        // socket, not a service — an unaccepted connect just sits in the
        // backlog), but `crate::serve` is not spawned until this channel has
        // fired. Serving first would let `await_ready` — and any harness probing
        // `/v1/status` for "up" — take a 200 carrying `NodeStatus::default()`
        // (version "", no modules, height 0) as the node's real state while
        // genesis is still running behind it.
        let (booted_tx, booted_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let actor = std::thread::Builder::new()
            .name("inproc-actor".into())
            .spawn(move || {
                run_actor(
                    build_host(),
                    status_modules,
                    ActorIo {
                        cmd_rx,
                        drain_rx,
                        status,
                        hub,
                        booted: booted_tx,
                    },
                )
            })
            .expect("spawn actor");
        // a dropped sender means genesis panicked; say that rather than block.
        booted_rx.recv().expect("in-proc actor died during genesis");

        let server = std::thread::Builder::new()
            .name("inproc-server".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("server runtime");
                rt.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener)
                        .expect("adopt the pre-bound harness listener");
                    let _ = crate::serve(listener, handle).await;
                });
            })
            .expect("spawn server");

        let daemon = Self {
            port,
            operator_token,
            server: Some(server),
            actor: Some(actor),
            drain,
            commands: Some(commands),
            status: published,
        };
        daemon.await_ready();
        daemon
    }

    /// Commit the host's ready calls and deliveries in ordinary empty blocks.
    /// The reply arrives after state/status and block notifications publish;
    /// no wall-clock wait or synthetic Program origin is involved.
    pub fn drain_ready_work(&self) -> Result<BlockSummary, String> {
        let (reply, result) = oneshot::channel();
        self.drain
            .unbounded_send(reply)
            .map_err(|_| "in-proc actor stopped".to_string())?;
        futures::executor::block_on(result)
            .map_err(|_| "in-proc drain reply dropped".to_string())?
    }

    /// the port the client surface listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// the actor's command lane, for an in-process component that speaks
    /// [`NodeCommand`] rather than http — the node's own collaboration pump is
    /// the one that does.
    ///
    /// **Every clone must be dropped before this daemon is.** A live sender
    /// holds the command channel open, and [`Drop`] joins the actor thread —
    /// which does not return while a sender exists. That is a HANG, not a
    /// failure. A component that owns one until a lane closes (the pump: close
    /// its wake lane and await it) is the shape that gets this right.
    pub fn commands(&self) -> mpsc::Sender<NodeCommand> {
        self.commands
            .clone()
            .expect("the command lane is taken only as this daemon is dropped")
    }

    /// the status cell the actor publishes into at every committed block — the
    /// agreed clock as this node last settled it.
    pub fn status(&self) -> crate::StatusCell {
        self.status.clone()
    }

    /// the http base a CLI's `--node` flag (or `DUCKTAPE_NODE`) takes.
    pub fn node_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// this harness's operator credential — what a real node writes 0600 into
    /// its workspace, and what every MUTATING `/v1` route wants from a caller
    /// that is not signing as a person ([`crate::signed_req`]).
    pub fn operator_token(&self) -> &str {
        &self.operator_token
    }

    /// the header pair a write carries, ready to set on a request.
    pub fn write_header(&self) -> (&'static str, &str) {
        (crate::admin::ADMIN_TOKEN_HEADER, &self.operator_token)
    }

    /// a duckfs transport whose writes this node admits — the harness twin of
    /// what `NodeLink::files()` builds for a real daemon.
    pub fn files(&self) -> duckfs_client::http::HttpNode {
        let token = self.operator_token.clone();
        duckfs_client::http::HttpNode::new(self.node_url()).with_write_auth(std::sync::Arc::new(
            move |_method, _path, _body| {
                vec![(crate::admin::ADMIN_TOKEN_HEADER.to_string(), token.clone())]
            },
        ))
    }

    /// Block until the server thread's runtime is actually accepting.
    ///
    /// Liveness FIRST, then the probe — the same order `bin/simnode`'s harness
    /// needed, and for the same reason. Binding in [`Self::start`] means a
    /// stranger cannot answer *while we hold the socket*, but that is not
    /// unconditional: if the server thread dies (its runtime fails to build, or
    /// `serve` panics) the listener drops with it, the port frees, and this loop
    /// would keep polling it for the remaining 30s — long enough for another
    /// harness to bind it and answer 200, which this would report as our node
    /// being ready. Asking `JoinHandle::is_finished()` first makes a dead server
    /// a named failure instead.
    ///
    /// What remains a genuine wait is the runtime spinning up: a connect lands
    /// in the backlog immediately, but no response comes back until `serve` is
    /// polling.
    fn await_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let server = self.server.as_ref().expect("the server thread is running");
        loop {
            assert!(
                !server.is_finished(),
                "the in-proc server thread exited before answering /v1/status on \
                 port {}; its listener is gone, so anything answering that port \
                 now belongs to someone else",
                self.port
            );
            if nettest::http_status(self.port, "GET", "/v1/status") == Some(200) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "in-proc node never answered /v1/status on port {}",
                self.port
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for InProcDaemon {
    fn drop(&mut self) {
        // graceful shutdown over the wire (NOT a held handle clone — that would
        // keep the command channel's Sender alive and wedge the actor's drain
        // loop open, hanging the join below). the POST makes serve() return →
        // its sole handle drops → the command channel closes → both threads end,
        // so a caller's tempdir (dropped AFTER this) is removed only once the
        // host's qmdb handles are closed.
        // and the spare of that channel this harness keeps for in-process
        // components goes FIRST, for the same reason: any sender still alive
        // holds the actor's drain loop open past both joins below.
        self.commands.take();
        let _ = nettest::http_status_with(
            self.port,
            "POST",
            "/v1/admin/shutdown",
            &[(crate::admin::ADMIN_TOKEN_HEADER, &self.operator_token)],
        );
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
    }
}

/// the minimal node actor: one block per submit, queries answered inline — the
/// SAME [`NodeCommand`] contract `bin/noded`'s daemon serves, so a harness
/// proves the real submit/query/frame path rather than a hand-mirrored stub.
struct ActorIo {
    cmd_rx: mpsc::Receiver<NodeCommand>,
    drain_rx: mpsc::UnboundedReceiver<oneshot::Sender<Result<BlockSummary, String>>>,
    status: crate::StatusCell,
    hub: crate::StreamHub,
    booted: std::sync::mpsc::SyncSender<()>,
}

fn run_actor(mut host: Host, status_modules: Vec<String>, io: ActorIo) {
    let ActorIo {
        mut cmd_rx,
        mut drain_rx,
        status,
        hub,
        booted,
    } = io;
    let mut height: u64 = 0;
    futures::executor::block_on(async move {
        // the boot snapshot, then one publish per committed block — the SAME
        // publish-into-the-cell contract the real daemons serve. the signal
        // releases the caller to bind: the first status a client can reach is
        // this one, never the cell's empty default.
        publish_status(&status, &host, &status_modules, height);
        let _ = booted.send(());
        loop {
            let command = futures::select! {
                command = cmd_rx.next().fuse() => command,
                request = drain_rx.next().fuse() => {
                    let Some(reply) = request else { break; };
                    let result = drain_ready_work(&mut host, &mut height, &status_modules, &status, &hub).await;
                    let _ = reply.send(result);
                    continue;
                }
            };
            let Some(cmd) = command else {
                break;
            };
            match cmd {
                NodeCommand::Submit {
                    target,
                    payload,
                    origin,
                    reply,
                } => {
                    let result = commit(
                        &mut host,
                        &mut height,
                        &hub,
                        &status,
                        &status_modules,
                        Origin::External(origin),
                        Msg { target, payload },
                    )
                    .await;
                    let _ = reply.send(result);
                }
                // the signed-frame lane, FAITHFUL to the real daemon: the origin
                // is the frame's VERIFIED signer, never a caller claim. a
                // forged/tampered frame is a rejection, not a block.
                NodeCommand::SubmitFrame { frame, reply } => {
                    let result = match node::decode_frame(&frame) {
                        Ok((origin, msg)) => {
                            commit(
                                &mut host,
                                &mut height,
                                &hub,
                                &status,
                                &status_modules,
                                origin,
                                msg,
                            )
                            .await
                        }
                        Err(err) => Err(err.to_string()),
                    };
                    let _ = reply.send(result);
                }
                NodeCommand::Query { target, req, reply } => {
                    let result = host.query(&target, &req).await.map_err(|e| e.to_string());
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
                        .map_err(|e| e.to_string());
                    let _ = reply.send(result);
                }
            }
        }
    });
}

/// assemble + publish the harness's `/v1/status` snapshot (each module's root
/// read straight from the host) into the shared cell.
fn publish_status(status: &crate::StatusCell, host: &Host, status_modules: &[String], height: u64) {
    let modules = status_modules
        .iter()
        .map(|id| ModuleStatus {
            id: id.clone(),
            root: host
                .module_root(id)
                .map(|r| hex_root(&r))
                .unwrap_or_default(),
            category: ModuleCategory::of(id),
        })
        .collect();
    status.publish(NodeStatus {
        contract: crate::NODE_CONTRACT,
        version: env!("CARGO_PKG_VERSION").into(),
        root_hash: hex_root(&host.root_hash()),
        height,
        // the harness never arms a `ConsensusTimePolicy` — height-is-time.
        consensus_time: height,
        consensus_time_unit: crate::ConsensusTimeUnit::Height,
        modules,
        public_key: status.current().public_key,
        chain_id: String::new(),
        operations: Default::default(),
    });
    // no mesh: height only, no roles or epoch — same as the embedded daemon.
    status.publish_peers(crate::PeersStanding {
        height,
        ..Default::default()
    });
}

/// apply one op as its own block, advancing `height` only on a clean commit — a
/// rejected op was never a block, so the height (and the reply) reports its
/// consensus fate faithfully.
async fn commit(
    host: &mut Host,
    height: &mut u64,
    hub: &crate::StreamHub,
    status: &crate::StatusCell,
    status_modules: &[String],
    origin: Origin,
    msg: Msg,
) -> Result<BlockSummary, String> {
    let next = *height + 1;
    let ctx = BlockContext {
        height: next,
        consensus_time: next,
        origin,
    };
    match host.submit_at(ctx, msg).await {
        Ok(out) => {
            *height = next;
            publish_status(status, host, status_modules, *height);
            hub.publish_block(
                *height,
                hex_root(&out.root_hash),
                crate::BlockWake::from_dispatches(&out.dispatches),
            );
            Ok(BlockSummary {
                height: *height,
                root_hash: hex_root(&out.root_hash),
            })
        }
        Err(err) => Err(err.to_string()),
    }
}

async fn drain_ready_work(
    host: &mut Host,
    height: &mut u64,
    modules: &[String],
    status: &crate::StatusCell,
    hub: &crate::StreamHub,
) -> Result<BlockSummary, String> {
    while host
        .has_pending_work()
        .await
        .map_err(|error| error.to_string())?
    {
        let next = *height + 1;
        let out = host
            .submit_block(
                BlockContext {
                    height: next,
                    consensus_time: next,
                    origin: Origin::System,
                },
                Vec::new(),
            )
            .await
            .map_err(|error| error.to_string())?;
        *height = next;
        publish_status(status, host, modules, *height);
        hub.publish_block(
            *height,
            hex_root(&out.root_hash),
            crate::BlockWake::from_dispatches(&out.internal_dispatches()),
        );
    }
    Ok(BlockSummary {
        height: *height,
        root_hash: hex_root(&host.root_hash()),
    })
}
