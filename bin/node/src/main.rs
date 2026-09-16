//! a runnable multi-process ducktape node over REAL sockets.
//!
//! this is the in-sim N-validator simplex proof (consensus/tests/
//! simplex_agreed_order.rs) turned into an actual network: instead of N
//! `SimplexOrderer`s over ONE `p2p::simulated` network under the DETERMINISTIC
//! clock, each process here stands up its OWN live simplex `Engine` over a real
//! `authenticated::lookup` encrypted TCP mesh on the REAL tokio runtime, and
//! drives an `OrderedNode<SimplexOrderer>` over a `host::Host`.
//!
//! the machinery is REUSED verbatim: `consensus::SimplexOrderer::spawn` is
//! already generic over the runtime context + the three engine channel pairs, so
//! the only substrate that changes vs the sim is (a) `tokio::Runner` instead of
//! `deterministic::Runner` (the p2p actors live-lock under the deterministic clock),
//! (b) `lookup::Network` channels instead of `simulated::Network`, and (c) a
//! per-process `ContentStore`.
//!
//! payload dissemination is REAL: each process submits a DISTINCT op (node N
//! creates task `kN`), so a peer that finalizes another node's op-digest
//! has NO local bytes for it. `SimplexOrderer::spawn_with_resolver` wires a
//! `ConsensusRelay` that, at propose time, gossips the proposed frame's bytes to
//! all peers on the payload channel; every peer's STORE-ONLY drain caches them, so
//! when that digest finalizes the reporter resolves it locally and delivers it in
//! BFT order. content-addressing IS the verification (the drain re-hashes on
//! receipt). the relay gossip is one-shot, and quorum is a SUBSET — a validator
//! can finalize a view whose gossip it missed — so a lazy payload FETCH lane
//! backstops it: the resolver pulls missing bytes by digest from the tracked
//! mesh and fills the finalized slot instead of wedging the apply prefix. this
//! is what lets DISTINCT ops converge across processes with per-process stores
//! — quorum votes still cross the real TCP mesh to finalize.
//!
//! ## state-sync service and the sync-only joiner
//!
//! every validator also serves the statesync wire protocol on
//! `CHANNEL_STATE_SYNC`, answered between drains from its latest finalized
//! boundary — so responses are always block-consistent without locks. run with
//! `--sync-only` and the process joins the mesh WITHOUT a consensus engine,
//! pulls a manifest + every module from the bootstrapper over that channel,
//! rebuilds them against their consensus-committed roots, prints its composed
//! `synced root_hash=`, and exits 0 — the network-backed joiner path over real
//! sockets. membership note: `peer_seeds` is the AUTHORIZED MESH (everyone,
//! including sync-only joiners); `validator_seeds` (default: peer_seeds) is the
//! CONSENSUS participant set — the split that lets a non-validator sync.
//!
//! each validator prints its GENESIS root-hash at startup and its CONVERGED
//! root-hash once it has applied ALL validator ops. the demo script asserts every
//! process's genesis line agrees, every converged line agrees, and the sync-only
//! joiner's synced line equals the converged line.

use commonware_cryptography::Signer;
use commonware_runtime::{Metrics as _, Runner, Supervisor};

mod account_cli;
mod agent;
mod agent_cli;
mod chief_cli;
mod agent_plane;
mod airlock;
mod announce;
mod blob_fetch;
mod boot;
mod cli;
mod cli_args;
mod code_plane;
mod collab_cli;
mod collab_keys;
mod collab_pump;
mod compute;
mod config;
mod constants;
mod cred_cli;
// the enclave-operator verbs (`user cred inspect|seal`) — real TEE quote
// verification behind the opt-in `verify` feature. The verbs and their flags
// exist in every build; without the feature they refuse at dispatch.
mod cred_seal;
mod drain_actions;
mod executors;
mod explorer;
mod first_contact_join;
mod fs_cli;
mod gateway_plane;
mod gateway_routes;
mod host_reads;
mod host_resources;
mod host_state;
mod join_gate;
mod known_nodes;
#[cfg(test)]
mod main_tests;
mod mcp;
mod mesh_book;
mod mesh_lanes;
mod mesh_window;
mod module_cli;
mod netstack_governance;
mod node_http;
mod overlay_book;
mod plane_metrics;
mod reachability_plane;
#[cfg(test)]
mod reachability_plane_tests;
mod relay;
mod relay_runtime;
mod release_cli;
mod replica;
mod resource_limits;
mod rpc;
mod sandbox_cli;
mod services;
mod sync;
mod task_dump;
mod tty;
mod userkey_cli;
mod util;
mod validator;
mod voice;
mod voice_plane;
mod wallet_cli;
mod work_admission;
use crate::util::fatal;
use config::Resolved;
#[cfg(test)]
use directory::{DirQuery, DirReply, decode_reply, encode_query};
use duckfs_disk::SyncScratch;
#[cfg(test)]
use explorer::sealed_frame_block_row;
#[cfg(test)]
use noded::projection::project_root_op;
use recovery::Recovery;
#[cfg(test)]
use replica::promotion::{
    PromotionBoundary, PromotionBoundarySource, choose_promotion_boundary,
    joiner_manifest_fetch_retry,
};
#[cfg(test)]
use sdk::{Msg, StateRoot};
#[cfg(test)]
use sync::catchup::{apply_suffix_frames, apply_verified_suffix_frame};
#[cfg(test)]
use sync::serve::assert_floor_binds_view;
#[cfg(test)]
use util::hex;

fn main() {
    resource_limits::cap_malloc_arenas();
    // before any subscriber exists — it must precede the first descriptor this
    // process opens — so stderr carries the failure and the outcome is RECORDED
    // for `report_open_file_limit` to put in `daemon.log` once the sink is up.
    if let Err(err) = node::resource_limits::raise_open_file_limit() {
        eprintln!("[node] warning: could not raise open-file limit: {err}");
    }
    #[cfg(target_os = "macos")]
    hold_macos_activity();
    // Convert any terminal error into the same stable `FATAL:` marker the node
    // already prints for its other fatal paths (recovery, admission, promotion),
    // plus a non-zero exit. This closes the run-path boot failures (bind
    // conflict, config parse) that used to propagate as a bare `Error: …` the
    // desktop app's classify() didn't recognize — now the app surfaces the
    // reason immediately instead of inferring death. (Onboarding subcommands
    // still surface their own stderr via run_verb; the prefix is harmless there.)
    if let Err(err) = run() {
        // `node run` (and `service run`) installs its subscriber BEFORE it
        // resolves the config and binds, so a bad field or a port conflict
        // propagates back here with a perfectly good sink open — and used to
        // leave daemon.log ending at the previous run's last line. that
        // subscriber tees stderr, so the one event IS the CLI's last word
        // there too; a one-shot verb installs none, and for it stderr is the
        // whole record.
        let sink_installed = tracing::dispatcher::has_been_set();
        if sink_installed {
            tracing::error!(
                target: "ducktape::boot",
                event = "boot_fatal",
                error = %err,
                "FATAL: {err}"
            );
        } else {
            eprintln!("FATAL: {err}");
        }
        std::process::exit(1);
    }
}

/// macOS: opt this process out of App Nap for its whole life. the desktop
/// shell spawns the node detached but the child stays in the app's darwin
/// coalition, and the app hides to the menu bar with zero visible windows —
/// exactly the state macOS answers with timer coalescing and I/O throttling.
/// a 1s-block consensus follower cannot survive either. the option set
/// deliberately ALLOWS idle system sleep (a node must never turn a laptop
/// into a space heater); `LatencyCritical` additionally asks for full timer
/// precision. the returned token re-enables the nap when it deallocates, so
/// it is forgotten, never dropped.
#[cfg(target_os = "macos")]
fn hold_macos_activity() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical,
        &NSString::from_str("ducktape follows 1s consensus blocks"),
    );
    std::mem::forget(token);
}

/// The bare-`ducktape` screen: from nothing to a working node, in the order a
/// person actually types it. Every line here is a command that runs as written
/// on a machine with one network on it — no placeholders except the two that
/// are genuinely yours to choose.
///
/// Deliberately NOT the full verb tree (`--help` on any family is that), and
/// deliberately stops at "it is running": what comes after depends on what you
/// want the node FOR, and each of those verbs points at its own next step.
const GETTING_STARTED: &str = "\
Getting started:
  ducktape node init --name mynet     found your own network here
  ducktape node join <invite>         ...or join someone else's
  ducktape node run                   start it (^C checkpoints and exits)
  ducktape wallet new <you>           mint your user key
  ducktape account create --name <you>
                                      found your account on it (signed by that key)
  ducktape node status                height + root hash of the running node

Then, to run agents on it:
  ducktape service run compute        offer this host's sandbox, and enable it
  ducktape user cred add claude       log a provider in, on this node
  ducktape agent pty claude           attach a terminal to a sandboxed agent

Each verb's own --help carries the rest. `ducktape node list` shows every
network this machine is registered on; -n <chain-id> picks one when there is
more than one.";

/// What `/v1/status` and `ducktape -V` report as `version`:
/// `<cargo version>+<build stamp>`, e.g. `0.1.0+e6352411a` or
/// `0.1.0+e6352411a-<dirty digest>`. The package version alone is pinned at
/// v1 forever and could never tell two hosts' builds apart; the stamp is
/// `noded`'s `DUCKTAPE_BUILD` (short sha, plus a working-tree digest when
/// dirty) or `unknown` for a git-less build. Display only — nothing admits or
/// refuses on it.
fn build_version() -> String {
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        noded::services::build_identity_or_unknown()
    )
}

/// clap's `version` wants a `&'static str`; built once, however many times
/// the command is (completions, docs generation).
static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(build_version);

// clap owns parsing, help, usage errors (exit 2) and `-V/--version`; the
// `FATAL:` wrapper in `main` stays for runtime death.
#[derive(clap::Parser)]
#[command(
    name = "ducktape",
    about = "one workspace-network node and its operator tools",
    // clap prints "<name> <version>", so the version string must not repeat
    // the binary name. Same stamp as `/v1/status`, so `ducktape -V` on two
    // hosts is the same comparison.
    version = VERSION.as_str(),
    arg_required_else_help = true,
    // `arg_required_else_help` means a bare `ducktape` lands HERE, so this is
    // the one screen every new operator sees. A list of eight families does
    // not tell anyone what to type first; the shortest real path does.
    after_help = GETTING_STARTED,
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    family: Family,
}

/// the command families — every one a typed clap tree (the hand-rolled
/// parsers are gone; each family's grammar lives beside its handlers).
// parsed once on the stack and immediately consumed — variant size is noise.
#[allow(clippy::large_enum_variant)]
#[derive(clap::Subcommand)]
enum Family {
    /// run a workspace node, plus operator verbs (init, invite, join, ...)
    #[command(subcommand)]
    Node(cli_args::NodeCmd),
    /// user keys and signing (init/restore, sign-*, cred)
    #[command(subcommand)]
    User(userkey_cli::UserCmd),
    /// the account this user key belongs to: create, keys, name, profile
    Account(account_cli::AccountArgs),
    /// a workspace's named user-key wallets: mint, import, list, switch the
    /// active one
    Wallet(wallet_cli::WalletArgs),
    /// local loopback bindings for signed gateway routes
    #[command(subcommand)]
    Gateway(gateway_routes::GatewayCmd),
    /// the duckfs working-copy CLI
    #[command(subcommand)]
    Fs(fs_cli::FsCmd),
    /// offchain service daemons: what is signaling, and what you have enabled
    #[command(subcommand)]
    Service(services::ServiceCmd),
    /// sandboxed provider sessions (pty attach, sched runs) and run control
    /// (cancel, reassign)
    Agent(agent_cli::AgentArgs),
    /// cross-device agent collaboration: authenticated reads of a conversation
    Collab(collab_cli::CollabArgs),
    /// live code swaps: update, register, status
    #[command(subcommand)]
    Module(module_cli::ModuleCmd),
    /// the desktop app's release: manifest sign/verify, bundle signing through the airlock gateway
    #[command(subcommand)]
    Release(release_cli::ReleaseCmd),
    /// the stdio MCP server an agent runner spawns
    Mcp,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = <Cli as clap::Parser>::parse();
    match cli.family {
        // `fs` owns a 0/1/2 exit-code contract, so it exits directly (after
        // flushing the stream `cat` wrote to); `mcp` is the stdio server the
        // agent runner spawns and holds until its stdin closes.
        Family::Fs(cmd) => {
            let code = fs_cli::run(cmd);
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            std::process::exit(code.into());
        }
        Family::Mcp => {
            mcp::serve();
            Ok(())
        }
        // The `__egress-hook` subcommand lived here: an OCI createRuntime hook
        // podman invoked to install a run's firewall inside its netns. A
        // microVM has no netns to enter — its guest has no network device at
        // all, and reaches this host only through a vsock tunnel the host owns
        // both ends of — so there is no hook and nothing for one to install.
        Family::User(cmd) => userkey_cli::run(cmd),
        Family::Account(args) => account_cli::run(args),
        Family::Wallet(args) => wallet_cli::run(args),
        Family::Agent(args) => agent_cli::run(args),
        Family::Collab(args) => collab_cli::run(args),
        Family::Gateway(cmd) => gateway_routes::run(cmd),
        Family::Service(cmd) => services::run(cmd),
        Family::Module(cmd) => module_cli::run(cmd),
        Family::Release(cmd) => release_cli::run(cmd),
        Family::Node(cli_args::NodeCmd::Run(args)) => run_node_verb(args),
        Family::Node(cli_args::NodeCmd::Op(op)) => cli::run(op),
    }
}

/// `ducktape node run` — the canonical node-boot path.
fn run_node_verb(args: cli_args::RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = args.selector.config_path()?;
    let sync_only = args.sync_only;

    let log_ring = noded::LogRing::default();
    // ONE filter over stderr + the ring (the ws `logs` topic) + the node's own
    // `<workspace>/daemon.log` (no spawner tees for us any more). the old
    // two-filter setup defaulted stderr to `EnvFilter::from_default_env()`,
    // whose no-directive default is ERROR — so with RUST_LOG unset nothing
    // below error was ever recorded at all.
    let workspace = cfg_path.parent().unwrap_or(std::path::Path::new("."));
    noded::log::init(Some(log_ring.clone()), Some(workspace.join("daemon.log")));
    report_open_file_limit();

    let resolved = config::resolve(&cfg_path)?;
    // BEFORE any of the boot: a binary whose module world is not the one this
    // workspace was founded with cannot instantiate the network's components.
    // Without this guard the failure is a "type-checking export func `shape`"
    // deep inside the restore compose — hours of reading the wrong plane.
    // Refused by name, with no tolerance window: rebuild or re-found.
    config::guard_founding_binary(
        &resolved.service.workspace,
        &resolved.service.chain_id,
        noded::services::build_identity_or_unknown(),
        wasm_host::module_world_digest(),
    )?;
    run_node(resolved, cfg_path, sync_only, log_ring)
}

/// Put the startup open-file raise ([`main`]) in `daemon.log`, ONCE, now that
/// there is a subscriber to carry it.
///
/// This is not decoration. The raise runs before any sink exists, so its only
/// previous record was a stderr line nothing tees — and a raise that silently
/// left the process on the launchd default (`launchctl limit maxfiles` is
/// `256 unlimited`) is indistinguishable, from the log, from one that worked.
/// The node then dies minutes later with an `EMFILE` from whatever opened last,
/// and nothing in the file says why.
fn report_open_file_limit() {
    use node::resource_limits::{MINIMUM_OPEN_FILES, startup_outcome};

    let Some(outcome) = startup_outcome() else {
        return;
    };
    let limit = match outcome {
        Ok(limit) => limit,
        Err(error) => {
            tracing::warn!(
                target: "ducktape::node",
                reason = "open_file_limit_unraised",
                error = %error,
                "open-file limit left at the inherited default"
            );
            return;
        }
    };
    tracing::info!(
        target: "ducktape::node",
        soft_limit = limit.soft,
        hard_limit = limit.hard,
        raised = limit.raised,
        "open-file limit"
    );
    if !limit.is_sufficient() {
        tracing::warn!(
            target: "ducktape::node",
            reason = "open_file_limit_too_low",
            soft_limit = limit.soft,
            needed = MINIMUM_OPEN_FILES,
            "open-file limit is below what a node needs; expect EMFILE under load"
        );
    }
}

/// the browser gateway serves on a running node (never under `--sync-only`)
/// that configures it and has a real overlay to route through. The app API's
/// bind address is not a condition: the gateway is its own listener pinned to
/// 127.0.0.1, refuses loopback routes aimed at this node's API ports by port,
/// and its CSP excludes the API port — none of which depends on where `/v1`
/// is bound.
fn gateway_can_start(
    sync_only: bool,
    gateway_listen: Option<&str>,
    wireguard_listen: Option<std::net::SocketAddr>,
) -> bool {
    !sync_only && gateway_listen.is_some() && wireguard_listen.is_some()
}

/// stand up the real-socket node from `cfg` and run it until killed (validator)
/// or until state sync completes (`--sync-only`).
///
/// deliberately NOT `#[tokio::main]`: `tokio::Runner` owns its OWN tokio runtime,
/// and you cannot start a runtime from inside one. so `main` is sync and hands
/// off to `Runner::start`, which drives everything (including the engine's spawned
/// tasks) on the runtime it owns.
fn run_node(
    resolved: Resolved,
    // this daemon's own `node.toml` — kept whole rather than re-derived from
    // `Resolved`, because the invite mint reads and REWRITES the descriptor
    // beside it (see the `/v1/invite` wiring below).
    cfg_path: std::path::PathBuf,
    sync_only: bool,
    log_ring: noded::LogRing,
) -> Result<(), Box<dyn std::error::Error>> {
    let boot::env::BootEnv {
        signer,
        label,
        namespace,
        identity_chain_id,
        peers,
        validators,
        mesh_book,
        coordinated,
        listen,
        advertised,
        storage,
        rpc_listen,
        http_listen,
        gateway_listen,
        wireguard_listen,
        wireguard_key_file,
        invite_listen,
        invite_token,
        invite_wireguard,
        invite_fronts,
        coordination,
        coord_cap,
        workspace,
        primary_coordinator,
        coordinator_relay,
        wireguard_advertised,
        sync_candidates,
        chain_id,
        mesh_state_file,
        checkpoint_blocks,
        cadence,
        dev_demo,
        sandbox,
        compute_backend,
        sandbox_capacity,
        genesis,
    } = boot::env::derive(resolved, sync_only);

    // A node whose config says it can isolate runs, booting with no compute
    // plane, is the shape EVERY workspace predating the grant has — and the
    // one an operator lands in without asking. Silence here is how an upgrade
    // looks like a hang, so the NODE BOOT that takes the compute-less branch
    // says it plainly and names the fix. Deliberately not in `config::resolve`:
    // that runs for every verb reading a workspace, including `service run`,
    // which is not booting a node and offers to fix this on its next line.
    let configured_but_ungranted = sandbox.is_some() && compute_backend.is_none();
    if configured_but_ungranted {
        tracing::warn!(
            target: "ducktape::service",
            node = %label,
            reason = "compute_not_granted",
            "sandbox configured but the compute service is not enabled; this node will run no \
             provider work and announce no capabilities — enable it with `ducktape service \
             run compute`"
        );
    }

    // The LENDER twin of the same silence. An operator whose credentials are
    // registered and granted on chain lends exactly nothing without an airlock
    // daemon, and every other diagnostic still reads healthy: `user cred list`
    // shows the records, `gateway list` shows the route, and `service
    // list`/`status` render no airlock row at all (they fold signaling ∪
    // grants, and an ungranted, unstarted service is in neither). This line is
    // the only place that says so.
    if let Some(credentials) = crate::airlock::lending_without_a_grant(&storage, &workspace) {
        tracing::warn!(
            target: "ducktape::service",
            node = %label,
            credentials,
            reason = "airlock_not_granted",
            "credentials are registered but the airlock service is not enabled; nothing will \
             lend them and a borrower's session will not connect — enable it with `ducktape \
             service run airlock`"
        );
    }

    // There is NO sandbox probe here any more, and its absence is the point:
    // this process runs nothing in a sandbox. Both planes that did — compute's
    // headless runs and agent's interactive ptys — are separate daemons that
    // probe the runtime themselves before they signal. Probing here would have
    // made a missing hypervisor, or a missing guest image, a fatal BOOT error
    // on a node that never needed one.

    // THE MESH LISTENER, taken for a moment while a bind failure can still be
    // a sentence. Everything below this line runs inside commonware's runtime,
    // where the same failure is an unwinding panic in a worker thread.
    // Skipped — not assumed — for an overlay-only node, which opens no OS
    // socket for the mesh at all.
    if boot::mesh::binds_an_os_mesh_socket(&namespace, &advertised, wireguard_listen.is_some()) {
        boot::mesh::preflight_mesh_listen(listen)?;
    }

    let gateway_enabled = gateway_can_start(sync_only, gateway_listen.as_deref(), wireguard_listen);

    // the announce's own base, kept before `http_listen` moves into the bind.
    let announce_base = http_listen.as_deref().map(config::http_base_of);

    let boot::surfaces::Surfaces {
        rpc_listener,
        http_cmds,
        status,
        stream_hub,
        index,
        voice_requests,
        code_stage_requests,
        blobs,
        services,
        gateway_requests,
        gateway_commands,
        service_link,
        node_api_ports,
    } = boot::surfaces::bind(boot::surfaces::BindConfig {
        sync_only,
        label: &label,
        storage: &storage,
        // the config dir where gateway-routes.json lives (= storage in the dev
        // shape); a service daemon registers its loopback port there.
        workspace: &workspace,
        rpc_listen,
        http_listen,
        gateway_listen,
        gateway_enabled,
        log_ring,
        // the forge worktree lane's committer identity (agent-dogfood M1):
        // every run commit is authored by this node's signer (D2 — the author
        // is the agent).
        // the owner-gated admin namespace resolves ownership against this node's
        // own key; exposure is the operator's `DUCKTAPE_ADMIN` choice.
        node_key: signer.public_key().as_ref().to_vec(),
        signer: signer.clone(),
        admin_exposure: noded::AdminExposure::from_env(),
    })?;

    // THE LIVENESS HALF of the capability announce. Consent is submitted by the
    // verb that changes it (`service enable`/`disable`); this watches the other
    // input — whether each granted kind's daemon is still signaling — and
    // submits the corrected set when that changes. It runs on its own OS thread
    // and talks to this node over its own `/v1`, because it must BLOCK on a
    // settling submit and the host must never leave the runner thread.
    //
    // No http surface means no `/v1` to submit through and no daemon that could
    // have signaled in the first place, so there is nothing to watch.
    if let Some(base) = announce_base {
        announce::spawn(announce::Watch {
            base,
            node_key: signer.public_key().as_ref().to_vec(),
            workspace: workspace.clone(),
            services: services.clone(),
            capacity: sandbox_capacity.clone(),
        })?;
    }

    // run on commonware's OWN tokio runtime, rooted at our per-process storage dir.
    let storage_for_sync = storage.clone();
    // 15s instead of commonware's 60s default: this read/write deadline is
    // the mesh's only half-open detector — see `constants::MESH_IO_TIMEOUT`.
    let rt_cfg = commonware_runtime::tokio::Config::default()
        .with_storage_directory(storage)
        .with_read_write_timeout(constants::MESH_IO_TIMEOUT);
    let executor = commonware_runtime::tokio::Runner::new(rt_cfg);

    // the seam's stack handle (socket mode): one slot for the process,
    // created HERE so every consumer — the mesh context's backend, the
    // statesync plane's socket factory, and the reachability plane's effect
    // (which owns the writes) — holds the same one. in tun/fake mode it just
    // stays empty.
    let overlay_slot = overlay_net::userspace::StackSlot::new();

    executor.start(|context| async move {
        let boot::mesh::MeshHead {
            context,
            metrics,
            plane_monitor,
            plane_metrics,
            sync_monitor,
            sync_metrics,
            mesh_participants,
            status_public_key,
            sync_sources,
            sync_source,
            advertised_reach,
            network,
            mut oracle,
            quota,
        } = boot::mesh::build(
            context,
            signer.clone(),
            namespace.clone(),
            peers.clone(),
            validators.clone(),
            sync_candidates,
            listen,
            advertised,
            wireguard_listen.is_some(),
            overlay_slot.clone(),
        );
        // the observability cell: operations overlay live from the metrics
        // the moment they exist; boundary facts stay zeroed (honest —
        // nothing is served yet) until a role loop publishes its first
        // boundary. the boot snapshot stamps the process constants so a
        // pre-boundary read still carries version + mesh identity, and the
        // exposition source feeds /metrics + /v1/peers off the command lane
        // (`Context` has no Clone; a child shares the SAME registry, so its
        // encode() serves the identical exposition).
        status.wire_metrics(&metrics);
        stream_hub.wire_metrics(&metrics);
        // the retained stores' footprint, on its own slow background task:
        // the blobstore and the derived index keep every applied op payload
        // forever and nothing prunes either, so their size IS the operator's
        // only warning (#1309). Off the node's task by construction — a walk
        // of a directory holding one file per op must never ride the loop.
        noded::spawn_store_footprint_sampler(metrics.clone(), storage_for_sync.clone());
        // the collaboration half of the agent link: it reads committed
        // conversations as each binding's own scoped key and hands the daemon
        // what it may still deliver, then commits what the daemon reports back.
        // Here rather than inside a role loop because it is neither — it holds
        // no host and only ever talks to one over the same command lane the
        // http surface uses, so a promotion re-exec restarts it like any other
        // surface task.
        collab_pump::spawn(
            gateway_commands.clone(),
            status.clone(),
            service_link.as_ref(),
            workspace.clone(),
            &chain_id,
        );
        let exposition_context = context.child("exposition");
        status.wire_exposition(move || exposition_context.encode());
        // `/v1/invite` — the daemon mints its own invites. Minting FOLDS this
        // member's dial hint into the network descriptor and saves it, and
        // reads the persisted mesh for the fronts the blob carries: both are
        // this process's files, so a second process doing it races us over
        // them. The route hands the work to a blocking thread.
        let invite_config = cfg_path.clone();
        status.wire_invite_minter(move |ttl_days| {
            let (blob, notes) =
                cli::mint_invite_blob(&invite_config, ttl_days).map_err(|why| why.to_string())?;
            for note in notes {
                tracing::warn!(
                    target: "ducktape::join",
                    reason = note.reason(),
                    "the minted invite carries fewer paths than a full mesh would give it"
                );
            }
            Ok(blob)
        });
        // the netstack plane's operator trigger and its status field. Both
        // exist only where a reachability plane will: `wireguard_listen` is
        // exactly the condition every role's wiring branches on, so a node
        // without one names no backend rather than naming a plane it never
        // started.
        if wireguard_listen.is_some() {
            tokio::spawn(reachability_plane::observe_execution(metrics.clone()));
            let swap_metrics = metrics.clone();
            status.wire_netstack_swapper(move |request| {
                let metrics = swap_metrics.clone();
                Box::pin(async move {
                    use reachability_plane::SwapAnswer;
                    let answer = reachability_plane::swap_netstack(request).await;
                    reachability_plane::record_swap(&metrics, &answer);
                    // the route answers 200 with the new backend, or 409 with
                    // the reason — a swap no machine was ever asked for reads
                    // to the operator exactly like one a machine refused.
                    match answer {
                        SwapAnswer::Swapped(backend) => Ok(backend),
                        SwapAnswer::Refused(reason) | SwapAnswer::Unattempted(reason) => Err(reason),
                    }
                })
            });
            // and the GOVERNANCE trigger for the same plane, on its own task:
            // the module code registry carries the designated component as a
            // commitment record, and this reconciler converges the plane onto
            // it one block wake at a time. Deliberately not the module
            // boundary — that one is fail-closed and rides the drain, and the
            // reachability machine is per-node, root-hash-free, pre-genesis
            // networking (`netstack_governance`).
            //
            // The command sender is the http surface's own (the gateway plane
            // holds another clone): the reconciler READS committed state the
            // same way `/v1/query` does.
            tokio::spawn(netstack_governance::reconcile(
                label.clone(),
                metrics.clone(),
                gateway_commands.clone(),
                blobs.clone(),
                stream_hub.subscribe_blocks(),
            ));
        }
        status.publish(noded::NodeStatus {
            contract: noded::NODE_CONTRACT,
            version: build_version(),
            public_key: status_public_key.clone(),
            ..Default::default()
        });
        // the chain every chain-scoped user proof names; a boot fact, wired
        // once so no role loop's boundary publish has to carry it.
        status.wire_chain_id(identity_chain_id.clone());
        // The one moment `/v1/status` starts carrying a mesh identity, and the
        // only wait seam a supervisor has for it. The HTTP listener binds well
        // upstream of here, so "app surface listening" says nothing about
        // whether the identity is there yet — a daemon started on that marker
        // reads `public_key: ""` and exits FATAL
        // (`services::NOT_PUBLISHED_YET`). Logged HERE rather than at a role
        // loop's first boundary publish: this is where the key actually
        // appears, and every role passes through it.
        tracing::info!(target: "ducktape::node", "mesh identity published");
        // One process-wide bulk budget: the per-use planes retain separate
        // protocols, queues, sockets, and admission but cannot independently
        // saturate the same WireGuard link.
        let bulk_pacer = overlay_book::shared_bulk_pacer();
        // The `ducktape_dataplane_*` / `ducktape_statesync_serve_*` series
        // unregister when these handles drop — pin them to the whole node
        // future (both role arms await inside this block).
        let _plane_metrics = plane_metrics;
        let _sync_metrics = sync_metrics;

        if sync_only {
            boot::sync_only::run(
                context,
                &label,
                network,
                oracle,
                quota,
                &signer,
                mesh_participants,
                &validators,
                mesh_book.clone(),
                sync_sources,
                metrics.clone(),
                storage_for_sync,
                namespace,
                identity_chain_id,
                blobs,
                &index,
                &genesis,
                voice_requests,
            )
            .await;
            return;
        }

        // ---- a VALIDATOR: consensus engine + state-sync service -------------

        // recovery-aware boot FIRST: the app state (and with it the epoch to
        // respawn) must be known before the mesh wiring below decides which
        // epochs' channels to live on. everything here is local disk io.
        let mut recovery = match Recovery::open(context.child("recovery")).await {
            Ok(r) => r,
            Err(e) => {
                fatal!(label, "cannot open the recovery store: {e}");
            }
        };
        // code-registry swaps realize through the blob plane: replay, catch-up,
        // and the live drain (which lifts this off the recovery sink) all fetch
        // committed component bytes from the node's content-addressed store.
        recovery.set_code_source(std::sync::Arc::new(host_state::BlobCodeSource(
            std::sync::Arc::new(blobs.clone()),
        )));
        let manifest = match recovery.manifest() {
            Ok(m) => m,
            Err(e) => {
                fatal!(label, "recovery checkpoint is damaged: {e}");
            }
        };
        // breadcrumb between the surface binds and the mesh/plane wiring: a
        // long journal replay is silent local disk io, and a boot log that
        // ends at "rpc listening" is otherwise indistinguishable from a hang.
        tracing::info!(
            target: "ducktape::recovery",
            node = %label,
            checkpoint = if manifest.is_some() { "present" } else { "none" },
            "recovery store open"
        );
        let forge_repo = storage_for_sync.join("forge-repo");
        let duckfs_dir = storage_for_sync.join("duckfs");
        // boot sweep (#219): no sync attempt is in flight yet, so any leftover
        // `duckfs_scratch_a*` dir (a crashed attempt, or a promoted scratch
        // whose final removal was interrupted) is safe to remove. best-effort.
        SyncScratch::sweep_stale(&duckfs_dir);

        // ---- the JOINER / REPLICA: park on the mesh, bootstrap a boundary,
        // then FOLD the head (unified-node phase 2) ----
        //
        // Every key outside the immutable genesis set enters the role resolver.
        // A local checkpoint is only a recovery base; it cannot authoritatively
        // name the key's CURRENT role while the process was offline. The replica
        // path reads the latest committed manifest, remains resident when that
        // boundary grants resident standing, or returns a promotion baton when
        // it seats the key as a validator.
        let genesis_validator = validators.contains(&signer.public_key());
        if !genesis_validator {
            let baton = replica::run(
                context,
                network,
                &mut oracle,
                quota,
                &mesh_participants,
                mesh_book.clone(),
                sync_sources,
                sync_source,
                advertised_reach.clone(),
                status_public_key.clone(),
                signer.clone(),
                label.clone(),
                namespace.clone(),
                identity_chain_id.clone(),
                peers.clone(),
                validators.clone(),
                wireguard_listen,
                wireguard_key_file.clone(),
                primary_coordinator.clone(),
                coordinator_relay,
                wireguard_advertised.clone(),
                &invite_token,
                &invite_wireguard,
                invite_fronts,
                &coord_cap,
                workspace.clone(),
                chain_id.clone(),
                mesh_state_file.clone(),
                checkpoint_blocks,
                rpc_listener,
                http_cmds,
                gateway_requests,
                gateway_commands.clone(),
                node_api_ports,
                &stream_hub,
                index.clone(),
                metrics.clone(),
                status.clone(),
                voice_requests,
                blobs.clone(),
                overlay_slot.clone(),
                bulk_pacer.clone(),
                plane_monitor.clone(),
                storage_for_sync,
                recovery,
                &manifest,
                forge_repo.clone(),
                duckfs_dir,
                &genesis,
            )
            .await;
            // THE PROMOTION SEAT: the park loop returned the baton — the
            // validator role continues INSIDE this process, over the mesh
            // and planes the parked role already runs.
            validator::run_promoted(
                baton,
                oracle,
                metrics,
                status,
                status_public_key,
                signer,
                label,
                namespace,
                validators,
                coordinated,
                wireguard_listen,
                wireguard_key_file,
                primary_coordinator,
                wireguard_advertised,
                invite_listen,
                coordination,
                coord_cap,
                chain_id,
                mesh_state_file,
                advertised_reach,
                checkpoint_blocks,
                cadence,
                dev_demo,
                stream_hub,
                index,
                code_stage_requests,
                forge_repo,
                blobs,
                overlay_slot,
                bulk_pacer,
                plane_monitor,
                sync_monitor,
                workspace.clone(),
            )
            .await;
            return;
        }
        validator::run_validator(
            context,
            network,
            oracle,
            mesh_book,
            quota,
            metrics,
            status,
            advertised_reach,
            status_public_key,
            signer,
            label,
            namespace,
            identity_chain_id,
            peers,
            validators,
            coordinated,
            wireguard_listen,
            wireguard_key_file,
            primary_coordinator,
            wireguard_advertised,
            invite_listen,
            coordination,
            coord_cap,
            chain_id,
            mesh_state_file,
            checkpoint_blocks,
            cadence,
            dev_demo,
            rpc_listener,
            http_cmds,
            gateway_requests,
            gateway_commands,
            node_api_ports,
            stream_hub,
            index,
            voice_requests,
            code_stage_requests,
            blobs,
            overlay_slot,
            bulk_pacer,
            plane_monitor,
            sync_monitor,
            workspace,
            recovery,
            manifest,
            forge_repo,
            duckfs_dir,
            &genesis,
        )
        .await;
    });

    Ok(())
}
