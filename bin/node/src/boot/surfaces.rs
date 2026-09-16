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
    pub(crate) voice_requests: tokio::sync::mpsc::Receiver<noded::PresenceSessionRequest>,
    pub(crate) code_stage_requests: tokio::sync::mpsc::Receiver<noded::CodeStageRequest>,
    pub(crate) blobs: noded::blobs::BlobHandle,
    /// the volatile service-signaling catalog shared with the http surface —
    /// the live half of the capability announce (`grant ∩ hello`).
    pub(crate) services: noded::services::ServiceCatalog,
    pub(crate) gateway_requests: Option<tokio::sync::mpsc::Receiver<noded::GatewayJob>>,
    pub(crate) gateway_commands: futures::channel::mpsc::Sender<noded::NodeCommand>,
    /// the host-side session manager (a clone of the one on the http handle), so
    /// the term plane's control handler can spawn peer-attached sessions. `None`
    /// on a node that hosts no terminal plane (sync-only / no http surface).
    pub(crate) terminals: Option<noded::TerminalSessions>,
    /// the guest-side remote-session lane the term plane's client half drains.
    pub(crate) session_requests: tokio::sync::mpsc::Receiver<noded::SessionJob>,
    /// the guest-side session-id → host-node registry the http handle writes on
    /// a remote create. The term plane's inbound feeds gate on it: a session's
    /// chunks and command rows are accepted only from the peer that hosts it.
    pub(crate) remote_sessions: noded::RemoteSessions,
    /// the host's own browser-gateway base URL — the `via` a resolved credential
    /// routes through. Empty when no browser gateway is bound.
    pub(crate) local_gateway_via: String,
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
        std::io::ErrorKind::AddrInUse => format!(
            "the {surface} address {addr} is already taken — a node for this workspace is \
             probably already running (`ducktape node list`, `ducktape node status`); \
             otherwise change `{key}` in node.toml"
        ),
        _ => format!("cannot bind the {surface} on {addr}: {error} (`{key}` in node.toml)"),
    })
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
    // the guest-side remote-session lane: /v1/term/sessions with a `node` hands a
    // SessionJob here, drained by the term plane's client half (mirrors the
    // gateway lane). The host's own browser-gateway base URL is the `via` a
    // resolved credential routes through.
    let (session_lane, session_requests) = tokio::sync::mpsc::channel::<noded::SessionJob>(32);
    let local_gateway_via = gateway_listener
        .as_ref()
        .map(|(_, address)| format!("http://{address}"))
        .unwrap_or_default();
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
    let (voice_lane, voice_requests) =
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
        .with_presence(voice_lane)
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
    // the node-local, off-chain interactive terminal-session plane (lives on the
    // http handle like the stream hub — never consensus). Wired wherever the app
    // surface is served: not sync-only, and an http address configured. A parked
    // joiner/resident gets the plane too — its park loop already spawns the full
    // term plane (guest lane included), and that is the credential-lending guest
    // shape: a resident laptop routing a pty to a compute host must not need a
    // validator seat. Membership is not this gate's job: cross-node reach rides
    // the mesh session plane, which only has tunnels to nodes with standing, so
    // an unadmitted joiner's directed create dies in the lane, not here.
    //
    // This node SPAWNS NO PTY. What is wired here is the rings, the per-session
    // metadata and the admission entry points; the ptys themselves live in the
    // agent daemon (`ducktape service run agent`), which attaches over this
    // node's own ws and owns its own sandbox. So the gate is purely "is there an
    // app surface to serve it on" — no sandbox backend, no provider discovery,
    // no execution identity. With no daemon attached a create returns the
    // "requires an agent service" 503, still distinct from the "terminal
    // sessions are not enabled" 503 that means the plane is missing entirely.
    let terminals = if !sync_only && http_listen.is_some() {
        // the boot marker an operator (and the parked-joiner regression test)
        // looks for: the plane is WIRED. Whether it can serve is a second
        // question, answered by whether an agent daemon has attached.
        tracing::info!(target: "ducktape::term", "terminal_plane_ready");
        // minted fresh each boot and written 0600 beside node.toml; the agent
        // daemon reads it on every attach. A mint failure disables the plane
        // rather than handing the link out unguarded.
        let link_token = noded::services::mint_link_token(workspace)
            .inspect_err(|error| {
                tracing::error!(
                    target: "ducktape::service",
                    reason = "link_token_unwritable",
                    "the interactive plane will refuse every agent service: {error}"
                );
            })
            .ok();
        Some(noded::TerminalSessions::new(
            stream_hub.terminals(),
            stream_hub.term_commands(),
            link_token,
        ))
    } else {
        None
    };
    // the term plane's host side (control handler) takes a clone of the same
    // manager the http handle serves; the guest side drains the session lane.
    let http_handle = match terminals.clone() {
        Some(manager) => http_handle
            .with_terminals(manager)
            .with_session_lane(session_lane),
        None => {
            drop(session_lane);
            http_handle
        }
    };
    // the guest-side session→host registry, taken before the handle moves into
    // the surface thread: the http routes write it on a remote create, the term
    // plane's inbound feeds read it to bind a session's grains to its host.
    let remote_sessions = http_handle.remote_sessions().clone();
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
        voice_requests,
        code_stage_requests,
        blobs,
        services,
        gateway_requests: gateway_enabled.then_some(gateway_requests),
        gateway_commands,
        terminals,
        session_requests,
        remote_sessions,
        local_gateway_via,
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
}
