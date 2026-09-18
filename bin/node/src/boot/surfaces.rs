/// `run_node`'s out-of-runtime surface bring-up (phase P1): the listener
/// binds that must fail as a clean startup error rather than an async
/// surprise, plus the app-surface HTTP server's own OS thread and the
/// channels/stores every later phase pumps.
pub(crate) struct Surfaces {
    pub(crate) rpc_listener: Option<std::net::TcpListener>,
    pub(crate) http_cmds: futures::channel::mpsc::Receiver<noded::NodeCommand>,
    /// the `/v1/status` snapshot cell shared with the http surface: the role
    /// loop that owns the host publishes into it at every boundary it settles.
    pub(crate) status: noded::StatusCell,
    pub(crate) stream_hub: noded::StreamHub,
    pub(crate) index: std::sync::Arc<indexer::IndexStore>,
    pub(crate) presence_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    pub(crate) code_stage_requests: tokio::sync::mpsc::Receiver<noded::CodeStageRequest>,
    pub(crate) blobs: noded::blobs::BlobHandle,
    /// the volatile service-signaling catalog shared with the http surface —
    /// the live half of the capability announce (`grant ∩ hello`).
    pub(crate) services: noded::services::ServiceCatalog,
    pub(crate) gateway_requests: Option<tokio::sync::mpsc::Receiver<noded::GatewayJob>>,
    pub(crate) gateway_commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    /// the node ↔ agent-daemon link (a clone of the one on the http handle), so
    /// the collaboration pump can drive the messaging bus. `None` on a node
    /// that serves no app surface (sync-only / no http surface).
    pub(crate) service_link: Option<noded::ServiceLink>,
    /// the ports THIS node's own surfaces answer on (operator rpc, browser
    /// gateway, app-surface http), as actually bound. The gateway plane
    /// refuses a loopback route aimed at any of them: a member mapping a
    /// route to its own `/v1` would hand the whole mesh its unauthenticated
    /// node API.
    pub(crate) node_api_ports: Vec<u16>,
}

pub(crate) struct BindConfig<'a> {
    pub(crate) sync_only: bool,
    pub(crate) label: &'a str,
    pub(crate) storage: &'a std::path::Path,
    /// the config dir where `gateway-routes.json` lives (= `storage` in the dev
    /// shape). A serving daemon registers its loopback port there so the
    /// gateway proxy can find it; the file is re-read per request.
    pub(crate) workspace: &'a std::path::Path,
    pub(crate) rpc_listen: Option<String>,
    pub(crate) http_listen: Option<String>,
    pub(crate) gateway_listen: Option<String>,
    pub(crate) gateway_enabled: bool,
    pub(crate) log_ring: noded::LogRing,
    /// this node's consensus public key — the salt every owner PoP on the
    /// admin namespace is bound to.
    pub(crate) node_key: Vec<u8>,
    /// this node's own mesh-identity signer, wired onto the handle so
    /// `POST /v1/huddle/node-proof` can mint a `JoinHuddle.node_proof` for it.
    pub(crate) signer: commonware_cryptography::ed25519::PrivateKey,
    /// how the owner-gated admin namespace is exposed.
    pub(crate) admin_exposure: noded::AdminExposure,
}

/// Bind one of the node's listeners, saying WHICH surface and WHICH address
/// when it will not come up.
///
/// The bare `TcpListener::bind(addr)?` these replaced propagated the raw io
/// error, so starting a node twice — far and away the most common way to reach
/// this line — printed exactly `FATAL: Address already in use (os error 98)`:
/// no port, no surface, no idea which of the four listeners lost, and no hint
/// that the node you already have running is the reason.
///
/// `key` is the `node.toml` field, so the message ends with something to edit.
pub(crate) fn bind_listener(
    surface: &str,
    key: &str,
    addr: &str,
) -> Result<std::net::TcpListener, String> {
    std::net::TcpListener::bind(addr).map_err(|error| match error.kind() {
        std::io::ErrorKind::AddrInUse => {
            let holder = address_holder(addr);
            format!(
                "the {surface} address {addr} is already taken: {holder}; otherwise change \
                 `{key}` in node.toml"
            )
        }
        _ => format!("cannot bind the {surface} on {addr}: {error} (`{key}` in node.toml)"),
    })
}

/// Who holds a taken address, said as far as it is known. A second node is
/// only the answer when a LISTENER holds it: the port can equally be the
/// source port of an outbound connection — a `curl`, a peer dial — when it
/// sits in the kernel's ephemeral range, and blaming a node that is not
/// running sends the operator looking for nothing.
fn address_holder(addr: &str) -> String {
    use crate::reachability_plane::{PortHolder, tcp_port_holder};
    let port = addr
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok());
    const EPHEMERAL_RACE: &str = "the kernel hands ports in its ephemeral range (32768–60999 \
         by default) to outbound connections, so a listener there can lose this race at any \
         restart";
    match port.and_then(tcp_port_holder) {
        Some(PortHolder::Listener(Some(process))) => format!(
            "{process} is listening on it — if that is this workspace's node, it is already \
             running (`ducktape node list`, `ducktape node status`)"
        ),
        Some(PortHolder::Listener(None)) => {
            "a process this user cannot inspect is listening on it".to_string()
        }
        Some(PortHolder::Connection(Some(process))) => format!(
            "{process} holds it as the source port of an outbound connection, until that \
             connection closes — {EPHEMERAL_RACE}"
        ),
        Some(PortHolder::Connection(None)) => format!(
            "an outbound connection holds it as its source port — another user's, or one \
             already closed and waiting out TIME_WAIT — {EPHEMERAL_RACE}"
        ),
        None => "another process or a transient connection holds it".to_string(),
    }
}

/// the operator's active wallet PUBLIC key, if this workspace has a keystore —
/// the key whose account owns the admin namespace. Read without a password
/// (the key file carries its pubkey in the clear); a workspace with no wallet,
/// or one whose keystore cannot be read, boots operator-gated instead of
/// refusing to boot. Per workspace, shared by the CLI and the app: a wallet
/// is an identity ON this network.
pub(crate) fn operator_wallet_key(workspace: &std::path::Path) -> Option<Vec<u8>> {
    let path = keystore::wallet::active_user_key(workspace).ok()?;
    let key = keystore::userkey::read_user_key_file(&path).ok()?;
    Some(key.pubkey)
}

pub(crate) fn bind(config: BindConfig<'_>) -> Result<Surfaces, Box<dyn std::error::Error>> {
    let BindConfig {
        sync_only,
        label,
        storage,
        workspace,
        rpc_listen,
        http_listen,
        gateway_listen,
        gateway_enabled,
        log_ring,
        node_key,
        signer,
        admin_exposure,
    } = config;
    // the rpc listener binds OUTSIDE the runtime (plain std tcp on OS threads)
    // so a bind failure is a clean startup error, not an async surprise. a
    // JOINER binds too: the park loop pumps the same surface — a resident
    // serves local reads from its pre-synced boundary, a still-parked joiner
    // answers with a clear not-admitted error instead of a dead port.
    let rpc_listener = match rpc_listen.as_deref() {
        Some(addr) if !sync_only => Some(bind_listener("operator rpc", "rpc_listen", addr)?),
        _ => None,
    };
    let rpc_port = rpc_listener
        .as_ref()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port());
    // the http/ws app surface: same bind-early rule. the server itself runs on
    // its OWN plain-tokio OS thread (noded's exact split — the host never
    // leaves the commonware runner thread; http handlers only send
    // NodeCommands over the lane), so the pump below is its single consumer.
    let (http_handle, http_cmds, stream_hub) = noded::NodeHandle::channel_with_log_ring(log_ring);
    let gateway_listener = match (gateway_listen.as_deref(), http_listen.as_deref()) {
        (Some(addr), Some(_)) if !sync_only && gateway_enabled => {
            let address: std::net::SocketAddr = addr
                .parse()
                .map_err(|error| format!("invalid gateway_listen {addr:?}: {error}"))?;
            if address.ip() != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST) {
                return Err("gateway_listen must bind exactly 127.0.0.1".into());
            }
            let listener = bind_listener("browser gateway", "gateway_listen", addr)?;
            listener.set_nonblocking(true)?;
            let actual = listener.local_addr()?;
            tracing::info!(
                target: "ducktape::gateway",
                node = %label,
                listen = %actual,
                "gateway browser listening"
            );
            Some((listener, actual))
        }
        _ => None,
    };
    let gateway_port = gateway_listener.as_ref().map(|(_, actual)| actual.port());
    let (gateway_lane, gateway_requests) = tokio::sync::mpsc::channel::<noded::GatewayJob>(32);
    // the derived per-module index (noded's exact store, <storage>/index),
    // plus the blocks database the explorer reads: the pump folds sealed
    // blocks into it, boot heals it from verified state at sync/recovery
    // boundaries, a resident's follow arm heals it at every state-changing
    // boundary it serves, and the already-routed GET /v1/blocks +
    // /v1/index/* lanes light up through the handle below. an open failure
    // is fatal-with-remedy rather than a silent no-index run: the tier is
    // rebuildable, so the fix is always "delete <storage>/index". opened
    // BARE before hydration: the module set and index guests are the
    // network's, carried by its genesis, which a joiner fetches after these
    // surfaces are up — the boot's genesis hydration
    // (`host_state::hydrate_genesis`) converges them. a module the registry
    // admitted after genesis gets its database when the host composes.
    let index = noded::open_index_store::<&str>(storage, &[])?;
    // this node's operator credential: minted fresh each boot and written 0600
    // beside node.toml, exactly like the service link token. Minted on EVERY
    // boot, `DUCKTAPE_ADMIN=off` included — it is no longer the admin
    // namespace's alone, it is what this node's own daemons present to the
    // mutating `/v1` write gate (`noded::signed_req`), and turning the control
    // surface off must not leave the announce and lease writes with nothing to
    // show. The node key goes on below; everything else is decided here.
    let admin = noded::AdminConfig::minted(admin_exposure, workspace);
    stream_hub.prime(index.resume_height()?, String::new());
    // the presence hub's session lane: /v1/presence/ws asks for
    // sessions here. created up front because the app-surface thread starts
    // before the mesh exists; validator and resident paths drain it, while a
    // sync-only or overlay-less path drops it so the routes refuse promptly.
    let (presence_lane, presence_requests) =
        tokio::sync::mpsc::channel::<noded::PresenceSessionRequest>(8);
    // the module-code stage lane: POST /v1/admin/module-code/stage fans an
    // artifact out through the node's code plane. same shape as the realtime
    // lane — created up front, drained only where the validator spawns the
    // plane; elsewhere the receiver drops and the route answers 503.
    let (code_stage_lane, code_stage_requests) =
        tokio::sync::mpsc::channel::<noded::CodeStageRequest>(4);
    // point the http handle at this node's forge repo base (the same
    // `storage/forge-repo` the host materializes into) so the git upload-pack
    // (clone/fetch) route can open a repo READ-ONLY and serve its objects.
    let http_handle = http_handle
        // persist node-local blobs (op receipts, agent prompt pins) under
        // <storage>/blobstore so a daemon restart keeps serving them.
        .with_blob_root(storage.join("blobstore"))?
        .with_forge_repo(storage.join("forge-repo"))
        .with_index_store(index.clone())
        .with_presence(presence_lane)
        .with_code_stage(code_stage_lane)
        .with_node_signer(signer)
        // the duckfs workspace RPC's managed-checkout root (disk state, separate
        // from the module's own `<storage>/duckfs` dir).
        .with_duckfs_workspaces(storage.join("duckfs-workspaces"))
        // the owner-gated control namespace: this node's own key
        // salts the owner PoP, and the operator's active wallet key names the
        // account that may present one (identity binds no node to anyone);
        // the exposure is the operator's choice (default loopback). shutdown +
        // module-code staging live here, off the unauthenticated public
        // surface.
        .with_admin(noded::AdminConfig {
            node_key: Some(node_key.clone()),
            owner_key: operator_wallet_key(workspace),
            ..admin
        });
    let http_handle = if gateway_enabled {
        http_handle.with_gateway(gateway_lane)
    } else {
        drop(gateway_lane);
        http_handle
    };
    let http_handle = match gateway_listener.as_ref() {
        Some((_, address)) => http_handle.with_browser_gateway(*address),
        None => http_handle,
    };
    let blobs = http_handle.blob_handle();
    let gateway_commands = http_handle.command_sender();
    // the /v1/status snapshot cell, captured BEFORE the serve/drop match
    // consumes the handle: the role loop publishes into it, the http route
    // reads it without crossing the command lane.
    let status = http_handle.status_cell();
    // the volatile signaling catalog, shared with the http surface: a service
    // daemon's `POST /v1/services/hello` lands here, and the role loop's
    // capability announce intersects it with the user's grant. There is NO
    // provisioner and NO credential resolver on this side any more — both moved
    // into the compute daemon, which reaches this node over /v1 like any other
    // local client.
    let services = http_handle.services().clone();
    // the node ↔ agent-daemon link (lives on the http handle like the stream
    // hub — never consensus). Wired wherever the app surface is served: not
    // sync-only, and an http address configured. It carries the collaboration
    // messaging bus, and its 0600 secret is what the workspace-gated ws topics
    // stand on.
    //
    // This node runs no terminal: a terminal session is an independently
    // installed `ducktape-terminal` process reached through its signed gateway
    // route, and nothing here spawns, drives or interprets one.
    let service_link = if !sync_only && http_listen.is_some() {
        // minted fresh each boot and written 0600 beside node.toml; the agent
        // daemon reads it on every attach. A mint failure disables the link
        // rather than handing it out unguarded.
        let link_token = noded::services::mint_link_token(workspace)
            .inspect_err(|error| {
                tracing::error!(
                    target: "ducktape::service",
                    reason = "link_token_unwritable",
                    "the agent service link will refuse every daemon: {error}"
                );
            })
            .ok();
        Some(noded::ServiceLink::new(link_token))
    } else {
        None
    };
    let http_handle = match service_link.clone() {
        Some(link) => http_handle.with_service_link(link),
        None => http_handle,
    };
    // (like the rpc surface above, a joiner binds and the park loop pumps —
    // reads only until promotion re-execs this process into a validator.)
    let mut http_port = None;
    match http_listen.as_deref() {
        Some(addr) if !sync_only => {
            let listener = bind_listener("node HTTP API", "http_listen", addr)?;
            listener.set_nonblocking(true)?;
            http_port = listener.local_addr().ok().map(|address| address.port());
            tracing::info!(
                target: "ducktape::http",
                node = %label,
                listen = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(),
                "app surface listening"
            );
            let thread_label = label.to_string();
            let gateway_listener = gateway_listener.map(|(listener, _)| listener);
            let gateway_handle = http_handle.clone();
            std::thread::Builder::new()
                .name("app-surface".into())
                .spawn(move || {
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("app-surface tokio runtime")
                        .block_on(async move {
                            noded::node_work::spawn(http_handle.clone(), "runs".into());
                            if let Some(listener) = gateway_listener {
                                let listener = tokio::net::TcpListener::from_std(listener)
                                    .expect("adopt gateway browser listener");
                                tokio::spawn(async move {
                                    if let Err(error) =
                                        noded::serve_browser_gateway(listener, gateway_handle).await
                                    {
                                        tracing::error!(
                                            target: "ducktape::gateway",
                                            error = %error,
                                            "gateway browser server stopped"
                                        );
                                    }
                                });
                            }
                            let listener = tokio::net::TcpListener::from_std(listener)
                                .expect("adopt app-surface listener");
                            if let Err(e) = noded::serve(listener, http_handle).await {
                                tracing::error!(
                                    target: "ducktape::http",
                                    error = %e,
                                    "app surface server stopped"
                                );
                            }
                        });
                    // a client asked the surface to shut down (POST /v1/admin/shutdown) —
                    // mirror the rpc shutdown: exit the whole process gracefully.
                    tracing::info!(
                        target: "ducktape::node",
                        node = %thread_label,
                        "shutdown requested via app surface; exiting"
                    );
                    std::process::exit(0);
                })?;
        }
        // surface off: dropping the handle terminates the command stream; the
        // pump's select arm sees one None and then never polls it again.
        _ => drop(http_handle),
    }

    Ok(Surfaces {
        rpc_listener,
        http_cmds,
        status,
        stream_hub,
        index,
        presence_requests,
        code_stage_requests,
        blobs,
        services,
        gateway_requests: gateway_enabled.then_some(gateway_requests),
        gateway_commands,
        service_link,
        node_api_ports: [rpc_port, gateway_port, http_port]
            .into_iter()
            .flatten()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Starting a node twice is the commonest way anyone reaches a bind
    /// failure, and it used to print exactly `FATAL: Address already in use
    /// (os error 98)` — no port, no surface, no hint that the node already
    /// running is the reason. Every listener routes through here, so one test
    /// covers all four.
    #[test]
    fn a_taken_port_names_the_surface_the_address_and_the_node_already_running() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = held.local_addr().expect("addr").to_string();

        let why = bind_listener("operator rpc", "rpc_listen", &addr)
            .expect_err("the port is held for the length of this test");
        assert!(why.contains("operator rpc"), "which surface: {why}");
        assert!(why.contains(&addr), "which address: {why}");
        assert!(why.contains("rpc_listen"), "what to edit: {why}");
        assert!(
            why.contains("already running"),
            "and the reason it usually is: {why}"
        );
        assert!(
            !why.contains("os error"),
            "the errno is noise once the sentence exists: {why}"
        );
        // a listener holds it, and on linux `/proc` says whose: this process.
        assert!(why.contains("is listening on it"), "{why}");
        #[cfg(target_os = "linux")]
        assert!(
            why.contains(&format!("pid {}", std::process::id())),
            "which process: {why}"
        );
        drop(held);

        // a DIFFERENT failure must not borrow that explanation.
        let refused = bind_listener("node HTTP API", "http_listen", "203.0.113.1:9")
            .expect_err("that address is not ours to bind");
        assert!(
            !refused.contains("already running"),
            "an unassignable address is not a second node: {refused}"
        );
        assert!(refused.contains("http_listen"), "{refused}");
    }

    /// The race the issue measured: a port in the ephemeral range handed to an
    /// outbound connection as its source port. The kernel refuses the node's
    /// bind exactly as it does for a second node, and the sentence blamed one —
    /// an operator went looking for a node that was not running. This process
    /// dials itself, so its own client end holds the port.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_port_held_by_an_outbound_connection_does_not_blame_a_node() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let client = std::net::TcpStream::connect(server.local_addr().expect("server addr"))
            .expect("dial the server");
        let addr = client
            .local_addr()
            .expect("the client's source address")
            .to_string();

        let why = bind_listener("node HTTP API", "http_listen", &addr)
            .expect_err("the client's source port is held for the length of this test");
        assert!(why.contains(&addr), "which address: {why}");
        assert!(why.contains("http_listen"), "what to edit: {why}");
        assert!(
            !why.contains("already running"),
            "no node holds it, so the sentence must not blame one: {why}"
        );
        assert!(
            why.contains("source port of an outbound connection"),
            "what does hold it: {why}"
        );
        assert!(
            why.contains(&format!("pid {}", std::process::id())),
            "whose connection: {why}"
        );
        drop(client);
    }
}
