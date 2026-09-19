//! resolution of both config shapes into the one runnable form
//! (`Resolved`), plus the wireguard/advertised endpoint derivations.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use commonware_cryptography::{Signer as _, ed25519};
use commonware_p2p::Ingress;
use provider_host::{SandboxBackend, Vmm};

use workspace_config::identity::load_identity;
use workspace_config::node_toml::{
    DevSeedToml, NodeToml, RawNodeToml, SandboxToml, load_raw_node_toml,
};
use workspace_config::{
    Coordination, DEFAULT_BLOCK_TIME_MS, DEFAULT_CHECKPOINT_BLOCKS, Front, InviteToken,
    NetworkDescriptor, ReachDial, StoredInviteWireGuard, dialable, hex_bytes, ingress_of,
    load_coord_cap, load_invite_fronts, load_invite_token, load_invite_wireguard,
    validate_block_time_ms,
};

/// everything `run_node` needs, shape-independent.
///
/// A node is a service daemon's workspace PLUS the key it signs with and the
/// consensus/transport plumbing only a node runs. That "plus" is the whole
/// relationship, so it is spelled structurally: [`ServiceConfig`] is a MEMBER
/// here, never a parallel copy of the same six facts. Nothing re-derives
/// `storage_dir` or `chain_id` beside it — there is one of each in the type
/// graph, so the node view and the daemon view cannot disagree about where a
/// workspace's state lives (a parity test asserting they agree would be
/// asserting `x == x`).
///
/// The nesting is one-directional ON PURPOSE: `Resolved` reaches
/// `ServiceConfig`, never the reverse. A daemon that holds a `ServiceConfig`
/// has no route — no field, no accessor, no `Option` left `None` by convention
/// — back to `signer`.
#[derive(Debug)]
pub struct Resolved {
    /// the keyless facts, derived exactly once (by [`resolve_service`]'s own
    /// per-shape halves) and shared verbatim with every service daemon.
    pub service: ServiceConfig,
    pub signer: ed25519::PrivateKey,
    /// log prefix: "#<id>" for the dev shape, the identity's short hex
    /// otherwise.
    pub label: String,
    /// the network genesis namespace, or the dev shape's raw namespace.
    pub namespace: Vec<u8>,
    /// the authorized mesh set (unsorted here; the caller builds the ordered
    /// Set the mesh tracks).
    pub mesh: Vec<ed25519::PublicKey>,
    /// the genesis consensus participant subset.
    pub validators: Vec<ed25519::PublicKey>,
    /// (identity, dial ingress) pairs to dial at startup; never contains
    /// self. hostname ingresses stay hostnames — dialers re-resolve them.
    pub dial_hints: Vec<(ed25519::PublicKey, Ingress)>,
    /// reach targets that need the nat client: (target key, coordinator
    /// ingress, coordinator key). empty unless an invite carries Coordinated
    /// hints. the runtime rendezvous/hole-punches through the coordinator to
    /// each target, then authenticates the target's own key end-to-end.
    pub coordinated: Vec<(ed25519::PublicKey, Ingress, ed25519::PublicKey)>,
    pub listen: SocketAddr,
    /// this node's self-announced dial address. a HOSTNAME advertised stays a
    /// hostname all the way into the signed peer record, so a node behind a
    /// tunnel with a stable name never needs an address update — and it BOOTS
    /// even while its own name does not resolve.
    pub advertised: Ingress,
    pub rpc_listen: Option<String>,
    pub gateway_listen: Option<String>,
    /// the staged WireGuard reachability plane's advertised UDP endpoint
    /// (userspace socket backend); None = plane off (dev-seed harness
    /// configs only — the network shape requires it).
    pub wireguard_listen: Option<SocketAddr>,
    /// the DIRECT invite intro listener endpoint (`invite_listen`, defaulted
    /// from `wireguard_listen` + 1); `None` when the plane is off — or when
    /// this config can never mint a direct intro endpoint (no dialable
    /// underlay host), where binding it would only leave an unreachable
    /// wildcard socket. coordinated intros ride the plane's shared underlay
    /// socket regardless (see `reachability_plane.rs`).
    pub invite_listen: Option<SocketAddr>,
    /// where the node's X25519 WireGuard keypair persists (beside
    /// identity.key in the network shape).
    pub wireguard_key_file: PathBuf,
    /// dev-seed shape marker: gates the boot-time demo op + converged print
    /// (scaffolding a REAL network must not write into its genesis).
    pub dev_demo: bool,
    /// sealed blocks between recovery checkpoints.
    pub checkpoint_blocks: u64,
    /// the idle block cadence, and the view timers scaled from it.
    pub cadence: consensus::Cadence,
    /// the invite token a `join` stored beside the descriptor, if any — what a
    /// parked joiner announces in its first-contact intro. always `None` for
    /// the dev shape and for manual (token-less) joins.
    pub invite_token: Option<InviteToken>,
    /// the inviter's WireGuard bootstrap a `join` stored, if any — the tunnel
    /// the joining node brings up BEFORE any p2p. always `None` for the dev
    /// shape and for members.
    pub invite_wireguard: Option<StoredInviteWireGuard>,
    /// the inviter's offered member fronts a `join` stored, if any — the
    /// ADDITIONAL first-contact paths the joiner races alongside the inviter.
    /// Empty for the dev shape and for members.
    pub invite_fronts: Vec<Front>,
    /// the reachability plane's coordination privacy (per-network operational
    /// policy). `Private` (the default) requires a validator-issued `CoordCap`
    /// for a node outside the validator set; `Public` accepts any
    /// proof-of-possession. The dev shape is always `Private` (it never uses a
    /// real coordinator).
    pub coordination: Coordination,
    /// the validator-issued admission capability this node presents on every
    /// coordinator request (loaded from `coord.cap` beside the identity).
    /// `None` for a genesis validator (admitted by membership), the dev shape,
    /// or a node that has not been issued one.
    pub coord_cap: Option<nat_traversal::CoordCap>,
    /// the AMBIENT coordinator override (`NodeToml::primary_coordinator`),
    /// raw and unvalidated — resolved through `coordinator_ingress` at the
    /// point of use so a bad value DEGRADES (coordinated paths dark, loud
    /// log) rather than aborting boot. `None` = re-derive the compiled
    /// default, exactly like today.
    pub primary_coordinator: Option<String>,
    /// the TCP relay override (`NodeToml::coordinator_relay`), raw and
    /// unvalidated — consumed at the join-race wiring site so a bad value
    /// DEGRADES (relay fallback dark, honest terminal) rather than aborting
    /// boot, the `primary_coordinator` discipline. `None` = derive the relay
    /// from the ambient coordinator (its host at TCP/443).
    pub coordinator_relay: Option<String>,
    /// the WireGuard endpoint this node advertises in its signed mesh record,
    /// decided ONCE here (`resolved_wireguard_advertised`) — the same answer
    /// the invite blob hands out; `None` = no dialable underlay host, the
    /// plane runs endpoint-less and roams.
    pub wireguard_advertised: Option<Ingress>,
    /// the COMPUTE SERVICE's backend — `Some` only when both halves agree:
    /// the operator's `[sandbox]` table says HOW runs are isolated on this
    /// host, and the user's `services.toml` grant (`ducktape service enable
    /// compute`) says WHETHER this node runs any. `None` = no provider
    /// mesh, no oracle pool, no capability announce.
    ///
    /// Deliberately NOT the same value as `service.sandbox`: the interactive
    /// terminal plane and the airlock gateway key off the table alone, so a node
    /// whose operator wants pty sessions does not have to grant it a compute
    /// service. Decided once here so no boot site re-derives the predicate.
    pub compute_backend: Option<SandboxBackend>,
    /// the genesis wasm set and where its bytes are: what this node seeds its
    /// code registry from at block zero, and the directory it reads those
    /// components out of. Both shapes carry it — the network shape from its
    /// descriptor (the hashes are IN the genesis fingerprint), the dev shape
    /// derived from the files themselves.
    pub genesis: GenesisModules,
}

/// the genesis code set of a network, as a booting node needs it: WHICH
/// components (by id and content hash) and WHERE their bytes live.
///
/// The hashes are the consensus fact — a node whose genesis hashes
/// differently is on a different network — and `source` is merely where
/// those bytes are read from, so the two travel together but only one is
/// signed for.
#[derive(Debug)]
pub struct GenesisModules {
    /// id -> sha256 of the genesis deployment, for every wasm tenant.
    pub hashes: BTreeMap<String, [u8; 32]>,
    /// where the bytes come from.
    pub source: GenesisSource,
}

/// where a booting node's genesis wasm comes from.
#[derive(Debug)]
pub enum GenesisSource {
    /// the network shape: the workspace genesis file, pinned by the
    /// descriptor's `genesis` hash. Absent until a joiner's first boot fetches
    /// it off the mesh; a founder or member holds it from `init`/`join`.
    Workspace { file: PathBuf, hash: [u8; 32] },
    /// the dev shape: a founding set directory, whose files ARE the genesis
    /// code set (hashed at resolve — there is no descriptor).
    FoundingSet(PathBuf),
}

pub use workspace_config::component_path;

/// Everything a SERVICE DAEMON legitimately needs from its node's workspace —
/// and, being a member of [`Resolved`], everything the NODE knows about the
/// same six facts. There is no second copy to drift from.
///
/// The type is separate from `Resolved` because `Resolved` carries
/// `signer: ed25519::PrivateKey`, so every `ducktape service run <kind>` used to
/// load the node's consensus identity into the daemon's address space — and a
/// daemon holding that key never needs `/v1/submit` (which re-signs with the
/// node key precisely so a daemon needs none): it can sign frames itself. That
/// makes every authorization boundary drawn later on `/v1` decorative, because
/// the caller already holds the thing `/v1` exists to lend.
///
/// This type has no field a secret could live in, [`resolve_service`] never
/// opens `identity.key`, and the containment runs ONE WAY — `Resolved` holds a
/// `ServiceConfig`, never the reverse — so on the daemon path holding the
/// node's key is UNREPRESENTABLE rather than merely unused.
///
/// The node's own IDENTITY is deliberately absent too: a daemon that needs it
/// asks the node (`GET /v1/status`). The process that holds the key is the one
/// that answers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// the workspace base directory — where `identity.key`, `network.toml`,
    /// `wireguard.key`, `coord.cap` and `services.toml` (a daemon's grant) live:
    /// the network shape's config directory, the dev shape's `storage_dir`.
    /// Threaded into the node so a parked joiner's gate phase can persist a
    /// `coord.cap` delivered over its sealed `IntroReply::Admitted` ack via
    /// `save_coord_cap`.
    pub workspace: PathBuf,
    /// the node's state root; per-service roots and provider state hang off it
    /// too.
    pub storage_dir: PathBuf,
    /// this network's chain id — the descriptor's own `chain_id` field (network
    /// shape) or the raw configured namespace (dev shape, which has no
    /// fingerprint appended). NOT `Resolved::namespace`: the network shape's
    /// `namespace` is `genesis_namespace()`, i.e. `chain_id@fingerprint` — a
    /// DIFFERENT string. This is the exact string the desktop app records as
    /// `Workspace.chain_id` (the `init` verb's last stdout line), so modules
    /// that must agree with the app on "this network's id" (e.g. `identity`'s
    /// certificate domain separation) use this field, never `namespace`. It is
    /// also the daemon's signaling line and consent screen.
    pub chain_id: String,
    /// the node's HTTP surface: the ONLY transport a daemon has to it.
    pub http_listen: Option<String>,
    /// the `[sandbox]` table — HOW this host isolates a run. `Some` = provider
    /// runs spawn inside this backend and the node announces `sandbox_capacity`;
    /// `None` = consensus-only, and a daemon must refuse to serve because this
    /// host has no configured way to isolate a run. A bare host spawn is
    /// unrepresentable. NOT the grant-gated `Resolved::compute_backend`: a
    /// daemon reads its own grant from `services.toml`, at the one place that
    /// already does.
    pub sandbox: Option<SandboxBackend>,
    /// the capacity that table yields (probed host totals, per-key overrides
    /// winning). EMPTY for a consensus-only node. This one value is both the
    /// dispatch pool's ledger and the capability announce's resources.
    pub sandbox_capacity: BTreeMap<String, u64>,
}

/// Read a node config into the DAEMON's view of it.
///
/// The keyless HALF of [`resolve`] — not a twin of it: `resolve` builds its
/// `Resolved` around the very [`ServiceConfig`] these shape functions return, so
/// there is no second derivation of `storage_dir`, `chain_id` or the sandbox
/// table to keep in step. No path through here calls `load_identity`, so a
/// workspace whose `identity.key` is absent or unreadable still resolves, which
/// is exactly the property `the_service_path_never_reads_the_node_key` pins.
pub fn resolve_service(cfg_path: &Path) -> Result<ServiceConfig, String> {
    match load_raw_node_toml(cfg_path)? {
        (RawNodeToml::Network(raw), _) => {
            let base = absolute_runtime_path(cfg_path.parent().unwrap_or_else(|| Path::new(".")))?;
            // the descriptor is read for its chain id alone — the validator set,
            // the reach hints and the genesis fingerprint are consensus facts a
            // daemon has no use for. It is still VALIDATED here, by the same
            // loader the node uses: a daemon that signaled happily against a
            // descriptor its own node will never boot on would announce capacity
            // for a network that cannot exist.
            let descriptor = load_valid_descriptor(&base.join(&raw.network))?;
            service_network_shape(&base, &raw, &descriptor)
        }
        (RawNodeToml::DevSeed(raw), _) => service_dev_shape(&raw),
    }
}

/// Load a network descriptor and refuse the two states NO process may run on.
///
/// One loader, one refusal set, for the same reason there is one derivation of
/// the six shared facts: the daemon path and the node path must not be able to
/// disagree about whether a `network.toml` is runnable. Splitting these checks
/// is the same drift the [`ServiceConfig`] nesting exists to prevent, just
/// wearing a different hat.
fn load_valid_descriptor(path: &Path) -> Result<NetworkDescriptor, String> {
    let descriptor = NetworkDescriptor::load(path)?;
    if descriptor.validator_keys()?.is_empty() {
        return Err(format!("network {} has no validators", descriptor.chain_id));
    }
    if descriptor.modules.is_empty() {
        return Err(format!(
            "network {} has no modules — re-found it with `node init --modules <dir>`",
            descriptor.chain_id
        ));
    }
    Ok(descriptor)
}

/// THE network-shape derivation of the six shared facts. `resolve_service`
/// returns this; `resolve` embeds it. Nothing else computes them.
fn service_network_shape(
    base: &Path,
    raw: &NodeToml,
    descriptor: &NetworkDescriptor,
) -> Result<ServiceConfig, String> {
    let (sandbox, sandbox_capacity) = resolve_sandbox(base, raw.sandbox.as_ref())?;
    Ok(ServiceConfig {
        workspace: base.to_path_buf(),
        storage_dir: base.join(&raw.storage_dir),
        chain_id: descriptor.chain_id.clone(),
        http_listen: Some(raw.http_listen.clone()),
        sandbox,
        sandbox_capacity,
    })
}

/// THE dev-shape derivation of the same six facts, on the same terms.
fn service_dev_shape(raw: &DevSeedToml) -> Result<ServiceConfig, String> {
    let storage_dir = dev_storage_dir(raw)?;
    let (sandbox, sandbox_capacity) = resolve_sandbox(&storage_dir, raw.sandbox.as_ref())?;
    Ok(ServiceConfig {
        // the dev shape has no config directory; its per-process state dir
        // stands in as the workspace.
        workspace: storage_dir.clone(),
        storage_dir,
        // the dev shape's namespace carries no fingerprint suffix, so it IS
        // the chain id.
        chain_id: raw.namespace.clone(),
        http_listen: raw.http_listen.clone(),
        sandbox,
        sandbox_capacity,
    })
}

/// Gate the resolved sandbox backend on the user's compute grant.
///
/// Two independent facts, and the compute service needs both: the `[sandbox]`
/// table (HOW a run is isolated on this host, written by `init`/`join`) and a
/// `services.toml` grant for `compute` (WHETHER this node may run one, minted
/// by `ducktape service enable compute`). Absent either, this is `None` and
/// the node has no compute capacity — the flag-day replacement for a node
/// founded without the retired `node init --compute`.
fn gate_on_compute_grant(
    sandbox: Option<&SandboxBackend>,
    workspace: &Path,
) -> Result<Option<SandboxBackend>, String> {
    let Some(backend) = sandbox else {
        // no table at all: a consensus-only node, silently and by choice.
        return Ok(None);
    };
    // This is a pure decision: `resolve` runs for EVERY verb that reads a
    // workspace, so the "you are compute-less" warning belongs at the node
    // boot that actually takes the branch, not here. See `run_node_verb`.
    let granted = crate::services::grant_for(workspace, crate::services::COMPUTE_KIND)?.is_some();
    // the grant's TAGS are not carried out of here: the announce re-reads them
    // per tick (see `validator::announce`), so a copy latched at resolve time
    // could only ever go stale.
    Ok(granted.then(|| backend.clone()))
}

/// resolve the operator's `[sandbox]` table into the compute plane: `None`
/// (no table) = consensus-only node, no backend and no capacity;
/// `"firecracker"` (Linux) or `"vz"` (macOS) → the microVM backend over the
/// WORKSPACE's guest images and executors, with the probed host totals,
/// per-key overrides winning; any other runtime — "podman", "tart" and
/// "direct" included, there is no container backend and no bare spawn — is a
/// loud config error naming the audited adapters.
fn resolve_sandbox(
    workspace: &Path,
    sandbox: Option<&SandboxToml>,
) -> Result<(Option<SandboxBackend>, BTreeMap<String, u64>), String> {
    let Some(sandbox) = sandbox else {
        return Ok((None, BTreeMap::new()));
    };
    // capacity derivation: probed totals with the operator's per-key overrides
    // (`0` = probe) winning. the map is validated
    // through THE consensus rule (capability::validate_resources) before it
    // leaves this boundary: a zero override would otherwise pass boot, get
    // announced, and be rejected by the module — wedging the announcer's
    // in-flight latch with a false success log instead of erroring here,
    // loudly.
    let probed = || -> Result<BTreeMap<String, u64>, String> {
        let mut capacity = crate::host_resources::probe();
        if sandbox.cores != 0 {
            capacity.insert("cores".into(), sandbox.cores);
        }
        if sandbox.mem_gb != 0 {
            capacity.insert("mem_gb".into(), sandbox.mem_gb);
        }
        capability::validate_resources(&capacity).map_err(|e| format!("sandbox capacity: {e}"))?;
        for dimension in ["cores", "mem_gb"] {
            if !capacity.contains_key(dimension) {
                return Err(format!(
                    "sandbox capacity: could not determine {dimension}; set [sandbox] {dimension} explicitly"
                ));
            }
        }
        Ok(capacity)
    };
    let vmm = match sandbox.runtime.as_str() {
        "firecracker" => Vmm::Firecracker,
        "vz" => Vmm::Vz,
        other => {
            return Err(format!(
                "sandbox runtime: {other:?} is not \"firecracker\" (Linux) or \"vz\" (macOS) \
                 — provider runs never execute bare on the host"
            ));
        }
    };
    // The guest images and the agent CLIs are the workspace's own files, never
    // table keys: `<workspace>/guest/{vmlinux,rootfs.ext4}` and
    // `<workspace>/executors/`. Two networks on one host boot two guests out of
    // two directories, and a workspace copied whole carries its sandbox along.
    // `workspace` is already absolute (the config's directory, or the dev
    // shape's state dir), so a guest that never dials back cannot be a
    // relative path resolved against the wrong cwd.
    Ok((
        Some(workspace_config::sandbox_backend(workspace, vmm)),
        probed()?,
    ))
}

fn absolute_runtime_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|e| format!("current directory: {e}"))
}

/// the dev shape's per-process state dir — its storage AND its stand-in
/// workspace. ONE derivation, so the node view ([`resolve`]) and the daemon
/// view ([`resolve_service`]) can never disagree about where a dev node's
/// state lives.
fn dev_storage_dir(raw: &DevSeedToml) -> Result<PathBuf, String> {
    match &raw.storage_dir {
        Some(path) => absolute_runtime_path(Path::new(path)),
        None => Ok(std::env::temp_dir().join(format!("ducktape-{}", raw.id))),
    }
}

/// read + resolve a config file into its runnable form. paths inside the file
/// (network, key_file, storage_dir) resolve relative to the file's directory,
/// so a workspace directory is relocatable.
///
/// The node view IS the daemon view plus the key: both shape functions below
/// build their [`Resolved`] around the [`ServiceConfig`] that
/// [`resolve_service`]'s own halves produce, so every fact both processes need
/// is computed in exactly one place.
pub fn resolve(cfg_path: &Path) -> Result<Resolved, String> {
    match load_raw_node_toml(cfg_path)? {
        (RawNodeToml::Network(raw), _) => {
            let base = absolute_runtime_path(cfg_path.parent().unwrap_or_else(|| Path::new(".")))?;
            resolve_network_shape(&base, raw)
        }
        (RawNodeToml::DevSeed(raw), _) => resolve_dev_shape(raw),
    }
}

fn resolve_network_shape(base: &Path, raw: NodeToml) -> Result<Resolved, String> {
    let descriptor = load_valid_descriptor(&base.join(&raw.network))?;
    // the shared half, derived by the daemon path's own function: from here on
    // `service.workspace` IS `base` and `service.storage_dir` IS the node's
    // state root, with nothing beside them to disagree.
    let service = service_network_shape(base, &raw, &descriptor)?;
    let key_path = base.join(&raw.key_file);
    let signer = load_identity(&key_path).map_err(|e| {
        format!("{e} — run `ducktape node init` or `ducktape node join <invite>` first")
    })?;
    let me = signer.public_key();

    // non-empty by `load_valid_descriptor`, which BOTH paths run.
    let validators = descriptor.validator_keys()?;
    // one dial source of truth: reach_entries() folds bootstrap-synthesised
    // Direct hints in with the typed `reach` hints (their union). Direct
    // resolves to a mesh Ingress dialed directly; Coordinated routes are handed
    // to the nat client, which rendezvouses through the coordinator and
    // hole-punches to the target — but the target is still authenticated
    // end-to-end by its own key, so a coordinated peer is a real mesh member
    // either way.
    let mut bootstrap: Vec<(ed25519::PublicKey, Ingress)> = Vec::new();
    let mut coordinated: Vec<(ed25519::PublicKey, Ingress, ed25519::PublicKey)> = Vec::new();
    for (key, dial) in descriptor.reach_entries()? {
        match dial {
            ReachDial::Direct(ingress) => bootstrap.push((key, ingress)),
            ReachDial::Coordinated { coord, coord_key } => {
                coordinated.push((key, coord, coord_key))
            }
        }
    }
    // mesh = validators ∪ every reach identity (direct + coordinated). A
    // fresh network-shape joiner may be outside this set at genesis; it parks
    // until governance admits it (the gate rides the WireGuard-tunnel
    // doorbell, so a pre-admission joiner needs no mesh door — its
    // REAL key is re-tracked onto every member's mesh at its Redeem grant).
    let mut mesh = validators.clone();
    for (k, _) in &bootstrap {
        if !mesh.contains(k) {
            mesh.push(k.clone());
        }
    }
    for (k, _, _) in &coordinated {
        if !mesh.contains(k) {
            mesh.push(k.clone());
        }
    }

    let listen: SocketAddr = raw.listen.parse().map_err(|e| format!("listen: {e}"))?;
    let advertised = resolve_advertised(
        Some(&raw.advertised),
        listen,
        &descriptor.genesis_namespace(),
        &me,
    )?;
    let dial_hints = bootstrap.into_iter().filter(|(k, _)| *k != me).collect();
    let wireguard_listen: SocketAddr = raw
        .wireguard_listen
        .parse()
        .map_err(|e| format!("wireguard_listen: {e}"))?;
    let wireguard_listen = Some(wireguard_listen);
    let invite_listen = resolved_intro_listener(
        Some(&raw.advertised),
        &raw.listen,
        Some(&raw.invite_listen),
        raw.wireguard_advertised_value(),
        wireguard_listen,
    )?;
    let wireguard_advertised = resolved_wireguard_advertised(
        raw.wireguard_advertised_value(),
        Some(&raw.advertised),
        &raw.listen,
        wireguard_listen,
    )?;
    let compute_backend = gate_on_compute_grant(service.sandbox.as_ref(), &service.workspace)?;
    // the descriptor IS the genesis code set here: its hashes are already in
    // the namespace fingerprint, so the genesis file beside it only has to
    // match — and a joiner that lacks the file fetches it by that pin.
    let genesis = GenesisModules {
        hashes: descriptor.module_hashes()?,
        source: GenesisSource::Workspace {
            file: workspace_config::genesis_path(base),
            hash: descriptor.genesis_hash()?,
        },
    };

    Ok(Resolved {
        label: hex_bytes(&me.as_ref()[..4]),
        namespace: descriptor.genesis_namespace().into_bytes(),
        signer,
        mesh,
        validators,
        dial_hints,
        coordinated,
        listen,
        advertised,
        rpc_listen: Some(raw.rpc_listen),
        gateway_listen: Some(raw.gateway_listen),
        wireguard_listen,
        wireguard_key_file: base.join("wireguard.key"),
        invite_listen,
        dev_demo: false,
        checkpoint_blocks: raw.checkpoint_blocks,
        // the beat is a genesis fact, not node plumbing: it rides the
        // descriptor (and its fingerprint), so every member of a network runs
        // the same simplex clock by construction.
        cadence: consensus::Cadence::from_millis(descriptor.block_time_ms),
        invite_token: load_invite_token(base)?,
        invite_wireguard: load_invite_wireguard(base)?,
        invite_fronts: load_invite_fronts(base)?,
        coordination: descriptor.coordination(),
        // the reachability plane presents this on every coordinator request; a
        // genesis validator needs none (admitted by membership), a joiner is
        // issued one beside its identity.
        coord_cap: load_coord_cap(base)?,
        primary_coordinator: Some(raw.primary_coordinator),
        coordinator_relay: Some(raw.coordinator_relay),
        wireguard_advertised,
        service,
        compute_backend,
        genesis,
    })
}

fn parse_wireguard_listen(raw: Option<&str>) -> Result<Option<SocketAddr>, String> {
    raw.map(|a| {
        a.parse::<SocketAddr>()
            .map_err(|e| format!("wireguard_listen: {e}"))
    })
    .transpose()
}

/// parse an EXPLICIT `wireguard_advertised` into a dial ingress: it must be
/// dialable (a hostname is kept VERBATIM, resolved once at plane start, same
/// discipline as the mesh `advertised`).
fn parse_wireguard_advertised(raw: Option<&str>) -> Result<Option<Ingress>, String> {
    match raw {
        None => Ok(None),
        Some(a) => ingress_of(a)
            .map_err(|e| format!("wireguard_advertised: {e}"))?
            .map(Some)
            .ok_or_else(|| format!("wireguard_advertised addr {a:?} is not dialable")),
    }
}

/// the WireGuard endpoint this node ADVERTISES, decided once for both of its
/// consumers — the reachability plane's signed mesh record and the invite
/// blob — so a peer never learns one endpoint from the invite and another
/// (or none) from the mesh. an explicit `wireguard_advertised` wins verbatim;
/// otherwise the host the invite hands out ([`invite_wireguard_endpoint`]: a
/// concrete `wireguard_listen` IP, else `advertised`/`listen`) at the
/// WireGuard port. a node with no dialable underlay host at all (`advertised
/// = "overlay"`, unspecified binds) advertises NO endpoint: peers install its
/// tunnel without one and its own initiations complete it (WireGuard roams to
/// the authenticated source).
///
/// the plane used to derive this from `wireguard_listen` ALONE: two joiners
/// on one LAN, both bound `0.0.0.0` with a dialable `advertised`, both
/// advertised no endpoint, and neither could ever initiate the other's
/// tunnel — consensus ran over the underlay, only the code plane was dark.
fn resolved_wireguard_advertised(
    explicit: Option<&str>,
    advertised: Option<&str>,
    listen: &str,
    wireguard_listen: Option<SocketAddr>,
) -> Result<Option<Ingress>, String> {
    if let Some(explicit) = parse_wireguard_advertised(explicit)? {
        return Ok(Some(explicit));
    }
    let Some(wg) = wireguard_listen else {
        return Ok(None);
    };
    let Ok(Some(derived)) = invite_wireguard_endpoint(advertised, listen, wg, None) else {
        return Ok(None);
    };
    // a derived value that is not dialable (a port-0 bind) is endpoint-less,
    // not a config error — only an EXPLICIT value is held to that.
    ingress_of(&derived).map_err(|e| format!("wireguard endpoint: {e}"))
}

/// the DIRECT invite intro listener the plane binds: [`resolved_invite_listen`],
/// but only when this config can mint an invite that carries a direct intro
/// endpoint ([`endpoint_host`] — the exact predicate the minting side uses).
/// a node with no dialable underlay host (the desktop shape: `advertised =
/// "overlay"`, unspecified binds) hands joiners only the coordinated path, so
/// a kernel intro listener would sit unreachable by construction — while
/// tripping host firewall prompts (macOS asks about every wildcard bind).
fn resolved_intro_listener(
    advertised: Option<&str>,
    listen: &str,
    invite_listen: Option<&str>,
    wireguard_advertised: Option<&str>,
    wireguard_listen: Option<SocketAddr>,
) -> Result<Option<SocketAddr>, String> {
    let Some(wg) = wireguard_listen else {
        return Ok(None);
    };
    let mints_a_direct_intro = matches!(
        endpoint_host(advertised, listen, wg, wireguard_advertised),
        Ok(Some(_))
    );
    if !mints_a_direct_intro {
        return Ok(None);
    }
    resolved_invite_listen(invite_listen, wg).map(Some)
}

/// the invite intro listener endpoint: explicit `invite_listen`, else the
/// WireGuard listen address with the next port — one convention both the
/// minting side (what lands in the blob) and the serving side (what the
/// plane binds) derive identically.
pub fn resolved_invite_listen(
    raw: Option<&str>,
    wireguard_listen: SocketAddr,
) -> Result<SocketAddr, String> {
    match raw {
        Some(a) => a.parse().map_err(|e| format!("invite_listen: {e}")),
        None => {
            let port = wireguard_listen
                .port()
                .checked_add(1)
                .ok_or("wireguard_listen port has no successor for the intro default")?;
            Ok(SocketAddr::new(wireguard_listen.ip(), port))
        }
    }
}

/// WHO chose the front a minted invite carries. The mint turns on this
/// distinction and nothing else: a loopback front is a working same-box
/// invite when the operator WROTE that address, and a dead end handed to a
/// stranger when the node picked it up off a neighbouring config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontSource {
    /// the operator named this tunnel address: `wireguard_advertised =
    /// "host:port"`, or a concrete host in `wireguard_listen`.
    Named,
    /// nothing named a tunnel address, so `wireguard_advertised = "auto"`
    /// fell back to the p2p `advertised`/`listen` host — the only other
    /// address this config knows. It is a reuse, not a statement about
    /// where this node is reachable.
    Derived,
}

/// the front a minted invite hands a joiner, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteFront {
    /// the HOST the intro endpoint carries.
    pub host: String,
    /// the FULL `host:port` the WireGuard `endpoint` carries — verbatim from
    /// `wireguard_advertised` when named, else `host` at the bind port.
    pub endpoint: String,
    pub source: FrontSource,
}

impl InviteFront {
    /// a front NO stranger can ever dial that this node chose ITSELF. The
    /// operator named no tunnel address, so `auto` reused the p2p hint, and
    /// that hint is loopback — every joiner off this box resolves it to their
    /// own machine. The mint refuses on this rather than print a credential
    /// that only fails ninety seconds later, on someone else's box.
    ///
    /// A HOSTNAME is never loopback here: deciding that would mean resolving
    /// it, and this derivation dials nothing.
    pub fn is_derived_loopback(&self) -> bool {
        let derived = self.source == FrontSource::Derived;
        let loopback = self
            .host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
        derived && loopback
    }
}

/// the front a minted invite's UDP endpoints carry, decided ONCE for the host
/// and the endpoint together so the two can never disagree about which
/// address — or whose choice — they came from: an explicit
/// `wireguard_advertised` wins outright (it IS the truth once configured, and
/// VERBATIM including its port, because in the port-forwarded setup the key
/// exists for the external port can differ from the bind port); else the
/// WireGuard listen IP when it is concrete; else the advertised host (an
/// invite must hand the joiner an underlay address that reaches this machine —
/// the usual listen is unspecified, so `advertised` is the truth).
///
/// The first two are the operator's own words, the third is this function
/// reusing the only other address in the file — [`FrontSource`] keeps them
/// apart all the way to the mint.
///
/// `Ok(None)` = nothing in this config NAMES a dialable host (`advertised =
/// "overlay"`, unspecified binds): endpoint-less, which every caller already
/// handles — an invite drops its underlay endpoints and the intro listener
/// stays unbound. Only a config that is WRONG (an `advertised` that can never
/// be dialed, a malformed `wireguard_advertised`) is an `Err`; absence on the
/// error channel is what turned a single-box founder's `invite` into a
/// refusal.
pub fn invite_front(
    advertised: Option<&str>,
    listen: &str,
    wireguard_listen: SocketAddr,
    wireguard_advertised: Option<&str>,
) -> Result<Option<InviteFront>, String> {
    if let Some(ingress) = parse_wireguard_advertised(wireguard_advertised)? {
        let (host, endpoint) = match ingress {
            Ingress::Socket(addr) => (addr.ip().to_string(), addr.to_string()),
            Ingress::Dns { host, port } => (host.to_string(), format!("{host}:{port}")),
        };
        return Ok(Some(InviteFront {
            host,
            endpoint,
            source: FrontSource::Named,
        }));
    }
    let named_bind_host = !wireguard_listen.ip().is_unspecified();
    let (host, source) = if named_bind_host {
        (wireguard_listen.ip().to_string(), FrontSource::Named)
    } else {
        let Some(dial) = dialable(advertised, listen)? else {
            return Ok(None);
        };
        // strip the port: `host:port` or `[v6]:port`.
        let host = match dial.rsplit_once(':') {
            Some((host, _)) => host.trim_matches(['[', ']']).to_string(),
            None => dial,
        };
        (host, FrontSource::Derived)
    };
    let endpoint = format!("{host}:{}", wireguard_listen.port());
    Ok(Some(InviteFront {
        host,
        endpoint,
        source,
    }))
}

/// the HOST [`invite_front`] names, for the callers that need only that.
pub fn endpoint_host(
    advertised: Option<&str>,
    listen: &str,
    wireguard_listen: SocketAddr,
    wireguard_advertised: Option<&str>,
) -> Result<Option<String>, String> {
    Ok(invite_front(advertised, listen, wireguard_listen, wireguard_advertised)?.map(|f| f.host))
}

/// the FULL `host:port` [`invite_front`] names, for the callers that need
/// only that.
pub fn invite_wireguard_endpoint(
    advertised: Option<&str>,
    listen: &str,
    wireguard_listen: SocketAddr,
    wireguard_advertised: Option<&str>,
) -> Result<Option<String>, String> {
    Ok(
        invite_front(advertised, listen, wireguard_listen, wireguard_advertised)?
            .map(|f| f.endpoint),
    )
}

/// resolve the `advertised` config value into a dial ingress. the sentinel
/// `"overlay"` advertises this node's chain-derived WireGuard overlay address
/// (`ula_v6_member_addr(namespace, identity)`) at the mesh listen port — the
/// address peers can dial once a tunnel to this node is up, and the RIGHT
/// advertisement for a member with no dialable underlay address (NAT, zero
/// exposed ports). the overlay is IPv6, so it requires an IPv6 mesh listener
/// (`listen = "[::]:port"` accepts both families on a default dual-stack
/// host); a v4-only listener would never see the tunnel's SYNs.
fn resolve_advertised(
    raw: Option<&str>,
    listen: SocketAddr,
    namespace: &str,
    me: &ed25519::PublicKey,
) -> Result<Ingress, String> {
    match raw {
        Some("overlay") => {
            if !listen.is_ipv6() {
                return Err(format!(
                    "advertised = \"overlay\" needs an IPv6 mesh listener to accept tunnel \
                     traffic — set listen = \"[::]:{}\"",
                    listen.port()
                ));
            }
            let identity = wireguard::ValidatorIdentity::try_from(me.as_ref())
                .map_err(|e| format!("advertised: {e:?}"))?;
            let ula = wireguard::ula_v6_member_addr(namespace, identity);
            Ok(Ingress::Socket(SocketAddr::new(
                std::net::IpAddr::V6(ula),
                listen.port(),
            )))
        }
        // an explicitly-configured advertised that can never be dialed is a
        // config error; a hostname is kept VERBATIM (no boot-time DNS).
        Some(a) => ingress_of(a)
            .map_err(|e| format!("advertised: {e}"))?
            .ok_or_else(|| format!("advertised addr {a:?} is not dialable")),
        None => Ok(Ingress::Socket(listen)),
    }
}

/// the dev-seed shape: every peer dials every other through the
/// index-aligned `peer_addrs` list (the mesh has no address gossip).
fn resolve_dev_shape(raw: DevSeedToml) -> Result<Resolved, String> {
    // the beat first — it is the one field nothing upstream has checked. the
    // descriptor shapes go through `NetworkDescriptor::from_toml`/`unpack_invite`
    // and the founding flag is range-limited, but `DevSeedToml::block_time_ms`
    // is a plain Option<u64> read straight into the Cadence. Zero collapses
    // every simplex timer (idle hold, leader timeout, the 2x certification and
    // 10x retry) to Duration::ZERO — the hot spin the validator exists to
    // refuse. Same rule, same error, at this boundary too.
    let block_time_ms = raw.block_time_ms.unwrap_or(DEFAULT_BLOCK_TIME_MS);
    validate_block_time_ms(block_time_ms)?;
    // the shared half first: it owns the storage/workspace derivation and must
    // run before any field of `raw` is moved out below.
    let service = service_dev_shape(&raw)?;
    // the dev shape's per-process state dir stands in as its workspace, so its
    // grant file sits beside its storage — one rule for both shapes.
    let compute_backend = gate_on_compute_grant(service.sandbox.as_ref(), &service.workspace)?;
    let wireguard_listen = parse_wireguard_listen(raw.wireguard_listen.as_deref())?;
    let invite_listen = resolved_intro_listener(
        raw.advertised.as_deref(),
        &raw.listen,
        raw.invite_listen.as_deref(),
        raw.wireguard_advertised.as_deref(),
        wireguard_listen,
    )?;
    let id = raw.id;
    let namespace = raw.namespace;
    let peer_seeds = raw.peer_seeds;
    let validator_seeds = raw
        .validator_seeds
        .clone()
        .unwrap_or_else(|| peer_seeds.clone());
    // duplicates would otherwise panic much later at run_node's Set::try_from.
    for (kind, seeds) in [
        ("peer_seeds", &peer_seeds),
        ("validator_seeds", &validator_seeds),
    ] {
        let mut seen = std::collections::BTreeSet::new();
        for s in seeds {
            if !seen.insert(*s) {
                return Err(format!("duplicate seed {s} in {kind}"));
            }
        }
    }

    let key_of = |s: u64| ed25519::PrivateKey::from_seed(s).public_key();
    let mesh: Vec<_> = peer_seeds.iter().map(|s| key_of(*s)).collect();
    let validators: Vec<_> = validator_seeds.iter().map(|s| key_of(*s)).collect();

    // every peer's dial address, index-aligned with peer_seeds — the mesh
    // transport has no address gossip, so the FULL list must come from
    // config. a solo node needs none; a multi-node cluster without it is a
    // dead cluster, refused loudly here rather than parked silently.
    let me = ed25519::PrivateKey::from_seed(id).public_key();
    let dial_hints: Vec<(ed25519::PublicKey, Ingress)> = match raw.peer_addrs {
        None if peer_seeds.len() <= 1 => Vec::new(),
        None => {
            return Err(
                "a multi-node dev cluster needs peer_addrs (one address per peer_seeds entry)"
                    .into(),
            );
        }
        Some(addrs) => {
            if addrs.len() != peer_seeds.len() {
                return Err(format!(
                    "peer_addrs has {} entries but peer_seeds has {} — they are index-aligned",
                    addrs.len(),
                    peer_seeds.len()
                ));
            }
            let mut hints = Vec::with_capacity(addrs.len());
            for (seed, addr) in peer_seeds.iter().zip(addrs.iter()) {
                let socket: SocketAddr = addr
                    .parse()
                    .map_err(|e| format!("peer_addrs entry for seed {seed}: {e}"))?;
                let peer = key_of(*seed);
                // self-filter matches the Resolved.dial_hints contract: a
                // node must never dial (or statesync) itself.
                if peer != me {
                    hints.push((peer, Ingress::Socket(socket)));
                }
            }
            hints
        }
    };

    let listen: SocketAddr = raw.listen.parse().map_err(|e| format!("listen: {e}"))?;
    let advertised = resolve_advertised(
        raw.advertised.as_deref(),
        listen,
        &namespace,
        &ed25519::PrivateKey::from_seed(id).public_key(),
    )?;

    let wireguard_advertised = resolved_wireguard_advertised(
        raw.wireguard_advertised.as_deref(),
        raw.advertised.as_deref(),
        &raw.listen,
        wireguard_listen,
    )?;
    let gateway_listen = raw
        .gateway_listen
        .clone()
        .or_else(|| raw.http_listen.as_ref().map(|_| "127.0.0.1:0".to_string()));
    // the dev shape has no descriptor, so the FILES are its genesis code set:
    // whatever `<dir>/<id>.component.wasm` hashes to is what this node seeds.
    // LAST, because it is the only check that touches the disk — a config with
    // a typo'd `listen` must be told about the typo, not about the bundle.
    let founding_set = match &raw.modules {
        Some(dir) => PathBuf::from(dir),
        None => noded::services::founding_set()?,
    };
    let genesis = GenesisModules {
        hashes: workspace_config::Genesis::compose(&founding_set)?.module_hashes(),
        source: GenesisSource::FoundingSet(founding_set),
    };
    Ok(Resolved {
        signer: ed25519::PrivateKey::from_seed(id),
        label: format!("#{id}"),
        namespace: namespace.into_bytes(),
        mesh,
        validators,
        dial_hints,
        // the dev-seed shape never uses coordinated reach — direct sockets only.
        coordinated: Vec::new(),
        listen,
        advertised,
        // the dev shape has no identity.key directory; the wireguard key
        // lives with the node's other per-process state.
        wireguard_key_file: service.storage_dir.join("wireguard.key"),
        rpc_listen: raw.rpc_listen,
        gateway_listen,
        wireguard_listen,
        invite_listen,
        dev_demo: true,
        checkpoint_blocks: raw.checkpoint_blocks.unwrap_or(DEFAULT_CHECKPOINT_BLOCKS),
        cadence: consensus::Cadence::from_millis(block_time_ms),
        invite_token: None,
        invite_wireguard: None,
        invite_fronts: Vec::new(),
        // the dev shape wires direct sockets only — no real coordinator, so
        // the coordination mode defaults to Private and no cap is presented.
        coordination: Coordination::Private,
        coord_cap: None,
        primary_coordinator: raw.primary_coordinator,
        coordinator_relay: raw.coordinator_relay,
        wireguard_advertised,
        service,
        compute_backend,
        genesis,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{coordinator_ingress, load_or_generate_identity};

    const DELETED_CWD_CONFIGS: &str = "DUCKTAPE_TEST_DELETED_CWD_CONFIGS";

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ducktape-config-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    /// a stand-in genesis module set for the network-shape descriptors these
    /// tests resolve: a descriptor with NO modules is not a runnable network.
    fn fake_modules() -> Vec<crate::config::ModuleCode> {
        vec![crate::config::ModuleCode {
            id: "pages".into(),
            code_hash: "11".repeat(32),
        }]
    }

    /// a dev-shape genesis bundle under `dir`: one stub `<id>.component.wasm`
    /// per production wasm tenant, returned as the `modules = ...` line a
    /// dev-seed config must carry (its code set is derived from these files).
    /// Append it BEFORE any `[sandbox]` table — a scalar after a table header
    /// belongs to the table.
    fn fake_bundle(dir: &Path) -> String {
        let modules = dir.join("modules");
        std::fs::create_dir_all(&modules).expect("modules dir");
        for id in topology::TOPOLOGY.wasm_ids(topology::PRODUCTION) {
            std::fs::write(component_path(&modules, id), id.as_bytes()).expect("write component");
        }
        format!("modules = {:?}\n", modules.to_str().expect("utf8 path"))
    }

    /// the `modules` line for a dev-seed config whose resolve must fail BEFORE
    /// the bundle is ever read: the key is required by the parse, the directory
    /// is deliberately absent. A test using this and still passing is the proof
    /// that hashing runs after the cheap checks.
    const UNREAD_BUNDLE: &str = "modules = \"/no/such/bundle\"\n";

    #[test]
    fn a_hostname_advertised_boots_without_dns_and_stays_a_hostname() {
        // the tunnel case: a stable name whose IP moves (or does not resolve
        // right now) must neither block boot nor be frozen to one lookup —
        // it stays a DNS ingress that dialing peers re-resolve every attempt.
        let dir = tmp("dnsadvertised");
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        let d = NetworkDescriptor {
            chain_id: "net#44444444".into(),
            validators: vec![hex_bytes(me.public_key().as_ref())],
            bootstrap: vec![format!(
                "{}@definitely-not-resolvable.ducktape.invalid:443",
                hex_bytes(me.public_key().as_ref())
            )],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        };
        d.save(&dir.join("network.toml")).expect("save");
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[
                ("listen", "\"127.0.0.1:52250\""),
                ("advertised", "\"my-tunnel.example.com:443\""),
            ]),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("hostnames never block boot");
        assert!(
            matches!(&r.advertised, Ingress::Dns { port: 443, .. }),
            "advertised stays a hostname: {:?}",
            r.advertised
        );
        // the unresolvable bootstrap hint is KEPT as a hostname too (self is
        // filtered from dial_hints, so check via the descriptor directly).
        let entries = d.bootstrap_entries().expect("hints parse");
        assert!(
            matches!(&entries[0].1, Ingress::Dns { port: 443, .. }),
            "hint stays a hostname: {:?}",
            entries[0].1
        );
    }

    #[test]
    fn the_mesh_carries_no_derived_lobby_identity() {
        // the derived lobby transport identity is RETIRED — the
        // tracked mesh is exactly the descriptor's real identities, nothing
        // derivable from the namespace alone.
        let dir = tmp("lobbymesh");
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        let d = NetworkDescriptor {
            chain_id: "net#33333333".into(),
            validators: vec![hex_bytes(me.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        };
        d.save(&dir.join("network.toml")).expect("save");
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[
                ("listen", "\"127.0.0.1:52240\""),
                ("advertised", "\"127.0.0.1:52240\""),
            ]),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve");
        assert_eq!(
            r.mesh,
            vec![me.public_key()],
            "mesh = the descriptor's real identities only"
        );
    }

    #[test]
    fn a_default_joiner_resolves_without_being_dialable() {
        // the zero-config joiner contract: the generated defaults (overlay
        // advertised, dual-stack listen, plane on) must resolve for a node
        // with no dialable underlay address at all — the joiner only ever
        // dials OUT to the descriptor's reach hints, so nothing may demand
        // it be reachable itself.
        let dir = tmp("nonwgjoin");
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        let founder = ed25519::PrivateKey::from_seed(7).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "net#44444444".into(),
            validators: vec![hex_bytes(founder.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        };
        d.add_bootstrap(&founder, "203.0.113.7:41000");
        d.save(&dir.join("network.toml")).expect("save");
        std::fs::write(dir.join("node.toml"), network_shape_toml(&[])).expect("write");
        let r = resolve(&dir.join("node.toml"))
            .expect("a joiner with no advertised and a non-dialable listen must resolve");
        assert_eq!(r.signer.public_key(), me.public_key());
        assert_eq!(r.dial_hints.len(), 1, "it dials the founder's hint");
    }

    #[test]
    fn network_shape_resolves_membership_and_bootstrap() {
        let dir = tmp("resolve");
        let key_path = dir.join("identity.key");
        let (me, _) = load_or_generate_identity(&key_path).expect("keygen");
        let other = ed25519::PrivateKey::from_seed(9).public_key();
        let mut d = NetworkDescriptor {
            chain_id: "net#11223344".into(),
            validators: vec![
                hex_bytes(me.public_key().as_ref()),
                hex_bytes(other.as_ref()),
            ],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        };
        d.add_bootstrap(&other, "127.0.0.1:52200");
        d.save(&dir.join("network.toml")).expect("save descriptor");
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[
                ("listen", "\"127.0.0.1:52201\""),
                ("advertised", "\"127.0.0.1:52201\""),
            ]),
        )
        .expect("write node.toml");

        let r = resolve(&dir.join("node.toml")).expect("resolve");
        // the running namespace is the chain-id plus the genesis fingerprint.
        assert_eq!(r.namespace, d.genesis_namespace().into_bytes());
        assert!(String::from_utf8_lossy(&r.namespace).starts_with("net#11223344@"));
        assert_eq!(r.validators.len(), 2);
        // exactly the validators — no derived lobby identity any more (the
        // join gate rides the tunnel doorbell).
        assert_eq!(r.mesh.len(), 2);
        // self never appears in dial_hints; the other member does.
        assert_eq!(r.dial_hints.len(), 1);
        assert_eq!(r.dial_hints[0].0, other);
        assert!(!r.dev_demo);
        assert_eq!(r.signer.public_key(), me.public_key());
        assert_eq!(r.service.storage_dir, dir.join("storage"));
        // the workspace base is the config directory — where a joiner would
        // persist a `coord.cap` delivered over its Admitted gate reply.
        assert_eq!(r.service.workspace, dir);
        // the genesis code set comes STRAIGHT off the descriptor (its hashes
        // are already in the namespace fingerprint), and its bytes are the
        // workspace genesis file's, pinned by the descriptor's hash.
        assert!(
            matches!(
                &r.genesis.source,
                GenesisSource::Workspace { file, hash }
                    if *file == dir.join("genesis") && *hash == [0xabu8; 32]
            ),
            "{:?}",
            r.genesis.source
        );
        assert_eq!(r.genesis.hashes["pages"], [0x11u8; 32]);
        assert_eq!(r.genesis.hashes.len(), fake_modules().len());
    }

    #[test]
    fn relative_network_config_yields_absolute_runtime_paths() {
        let cwd = std::env::current_dir().expect("current directory");
        let workspace = tempfile::Builder::new()
            .prefix("ducktape-relative-config-")
            .tempdir_in(&cwd)
            .expect("workspace in current directory");
        let dir = workspace.path();
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        NetworkDescriptor {
            chain_id: "relative#11223344".into(),
            validators: vec![hex_bytes(me.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[
                ("listen", "\"127.0.0.1:52201\""),
                ("advertised", "\"127.0.0.1:52201\""),
            ]),
        )
        .expect("write node.toml");

        let relative_config = dir
            .strip_prefix(&cwd)
            .expect("workspace is below cwd")
            .join("node.toml");
        let resolved = resolve(&relative_config).expect("resolve relative config");

        assert_eq!(resolved.service.storage_dir, dir.join("storage"));
        assert!(resolved.service.storage_dir.is_absolute());
        assert_eq!(resolved.service.workspace, dir);
        assert!(resolved.service.workspace.is_absolute());
    }

    #[test]
    fn dev_shape_relative_storage_is_absolute_from_launch_cwd() {
        let launch_cwd = std::env::current_dir().expect("current directory");
        let dir = tmp("devrelative");
        let bundle = fake_bundle(&dir);
        let raw: DevSeedToml = toml::from_str(&format!(
            "id = 7\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\n\
             peer_seeds = [7]\n\
             storage_dir = \"relative/storage\"\n{bundle}"
        ))
        .expect("parse dev config");
        let resolved = resolve_dev_shape(raw).expect("resolve relative storage");
        assert_eq!(
            resolved.service.storage_dir,
            launch_cwd.join("relative/storage")
        );
        assert!(resolved.service.storage_dir.is_absolute());

        let default_raw: DevSeedToml = toml::from_str(&format!(
            "id = 8\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\n\
             peer_seeds = [8]\n{bundle}"
        ))
        .expect("parse default dev config");
        let default = resolve_dev_shape(default_raw).expect("resolve default storage");
        assert_eq!(
            default.service.storage_dir,
            std::env::temp_dir().join("ducktape-8")
        );
        assert!(default.service.storage_dir.is_absolute());
    }

    #[test]
    fn the_dev_shape_refuses_a_zero_beat() {
        // "as fast as possible" in a harness, or a typo: every Cadence timer
        // is a multiple of the beat, so zero boots a node that spins views at
        // CPU speed with no line saying why. The descriptor shapes already
        // refuse it; this one must say the same thing.
        let dir = tmp("devzerobeat");
        let bundle = fake_bundle(&dir);
        let raw: DevSeedToml = toml::from_str(&format!(
            "id = 9\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\n\
             peer_seeds = [9]\nblock_time_ms = 0\n{bundle}"
        ))
        .expect("parse dev config");
        let err = resolve_dev_shape(raw).expect_err("a zero beat is not a cadence");
        assert!(err.contains("block_time_ms"), "{err}");

        // a beat under the drain-tick floor is refused too: this shape boots
        // the SAME pump_heartbeat/DRAIN_TICK-driven node, so 1ms is no more a
        // cadence here than it is at the descriptor boundary.
        let too_fast: DevSeedToml = toml::from_str(&format!(
            "id = 9\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\n\
             peer_seeds = [9]\nblock_time_ms = 1\n{bundle}"
        ))
        .expect("parse dev config");
        let err = resolve_dev_shape(too_fast).expect_err("1ms is below the drain tick");
        assert!(err.contains("block_time_ms"), "{err}");

        // the floor itself still resolves.
        let at_floor: DevSeedToml = toml::from_str(&format!(
            "id = 9\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\n\
             peer_seeds = [9]\nblock_time_ms = {}\n{bundle}",
            workspace_config::MIN_BLOCK_TIME_MS
        ))
        .expect("parse dev config");
        resolve_dev_shape(at_floor).expect("the floor is a cadence");
    }

    /// ISOLATED FROM THE PARALLEL SUITE ON PURPOSE — `make test` runs it, but
    /// in its own serial pass (`-- --ignored --test-threads=1`).
    ///
    /// This test re-execs the ~450 MB test binary as a subprocess. Doing that
    /// while 32 libtest threads are live made the WHOLE suite fail ~4 runs in
    /// 11, on a different test each time and always with an integrity verdict
    /// (`corrupt or wrong password` out of argon2+AEAD, `rejected the bytes as
    /// corrupt` out of a digest check). Bisection pinned it here: skip this one
    /// test and the suite goes 0/11; serialize the suite and it goes 0/4.
    ///
    /// The SPAWN is what matters, not the cwd trick — removing only the
    /// `set_current_dir`+`remove_dir` still left 1/6. So this test is the
    /// trigger for a load sensitivity, not the corruption itself; the
    /// underlying cause is unresolved and tracked in #887. Isolating it keeps
    /// its coverage while stopping it from poisoning unrelated tests.
    ///
    /// It cannot simply move to `bin/node/tests/`: `node-bin` has no lib
    /// target, so an integration test cannot reach `resolve()` at all.
    #[cfg(unix)]
    #[test]
    #[ignore = "re-execs the test binary; run serially — see #887"]
    fn absolute_configs_resolve_after_launch_cwd_is_deleted() {
        if let Ok(paths) = std::env::var(DELETED_CWD_CONFIGS) {
            let mut paths = paths.lines();
            let network_config = paths.next().expect("network config");
            let dev_config = paths.next().expect("dev config");
            let doomed_cwd = paths.next().expect("doomed cwd");

            std::env::set_current_dir(doomed_cwd).expect("enter doomed cwd");
            std::fs::remove_dir(doomed_cwd).expect("remove launch cwd");
            assert!(
                std::env::current_dir().is_err(),
                "cwd is genuinely unavailable"
            );

            let network = resolve(Path::new(network_config)).expect("absolute network config");
            assert!(network.service.storage_dir.is_absolute());
            let dev = resolve(Path::new(dev_config)).expect("absolute dev config");
            assert!(dev.service.storage_dir.is_absolute());
            return;
        }

        let network_dir = tmp("deleted-cwd-network");
        let (me, _) = load_or_generate_identity(&network_dir.join("identity.key")).expect("keygen");
        NetworkDescriptor {
            chain_id: "deleted-cwd#11223344".into(),
            validators: vec![hex_bytes(me.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        }
        .save(&network_dir.join("network.toml"))
        .expect("save descriptor");
        let network_config = network_dir.join("node.toml");
        std::fs::write(
            &network_config,
            network_shape_toml(&[
                ("listen", "\"127.0.0.1:52201\""),
                ("advertised", "\"127.0.0.1:52201\""),
            ]),
        )
        .expect("write network config");

        let dev_dir = tmp("deleted-cwd-dev");
        let dev_config = dev_dir.join("node.toml");
        std::fs::write(
            &dev_config,
            format!(
                "id = 0\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\npeer_seeds = [0]\n{}",
                fake_bundle(&dev_dir)
            ),
        )
        .expect("write dev config");
        let doomed_cwd = tmp("deleted-cwd-launch");

        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "config::resolve::tests::absolute_configs_resolve_after_launch_cwd_is_deleted",
                "--nocapture",
            ])
            .env(
                DELETED_CWD_CONFIGS,
                format!(
                    "{}\n{}\n{}",
                    network_config.display(),
                    dev_config.display(),
                    doomed_cwd.display()
                ),
            )
            .output()
            .expect("run isolated deleted-cwd test");
        assert!(
            output.status.success(),
            "child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_non_member_identity_resolves_as_a_pending_joiner() {
        let dir = tmp("nonmember");
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        let other = ed25519::PrivateKey::from_seed(3).public_key();
        let d = NetworkDescriptor {
            chain_id: "closed#00000000".into(),
            validators: vec![hex_bytes(other.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        };
        d.save(&dir.join("network.toml")).expect("save");
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[
                ("listen", "\"127.0.0.1:52202\""),
                ("advertised", "\"127.0.0.1:52202\""),
            ]),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("non-member resolves as a joiner");
        assert_eq!(r.signer.public_key(), me.public_key());
        assert!(!r.validators.contains(&me.public_key()));
        assert_eq!(r.validators, vec![other.clone()]);
        // no derived lobby door any more: the joiner's own key
        // enters the mesh at its Redeem grant, not at resolve time.
        assert_eq!(r.mesh, vec![other]);
    }

    #[test]
    fn dev_shape_hashes_its_modules_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("modules");
        std::fs::create_dir_all(&modules).expect("modules dir");
        for id in topology::TOPOLOGY.wasm_ids(topology::PRODUCTION) {
            std::fs::write(component_path(&modules, id), id.as_bytes()).expect("write component");
        }
        let cfg = dir.path().join("node.toml");
        std::fs::write(
            &cfg,
            format!(
                "id = 1\nnamespace = \"t\"\npeer_seeds = [1]\nlisten = \"127.0.0.1:0\"\n\
                 storage_dir = {:?}\nmodules = {:?}\n",
                dir.path().join("storage").to_str().expect("utf8 path"),
                modules.to_str().expect("utf8 path")
            ),
        )
        .expect("write node.toml");
        let r = resolve(&cfg).expect("resolve dev shape");
        assert!(
            matches!(&r.genesis.source, GenesisSource::FoundingSet(dir) if *dir == modules),
            "{:?}",
            r.genesis.source
        );
        assert_eq!(
            r.genesis.hashes["pages"],
            module_artifact::Artifact::module(b"pages".to_vec()).hash(),
            "each hash commits the whole deployment from disk"
        );
    }

    #[test]
    fn dev_shape_refuses_an_empty_module_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modules = dir.path().join("modules");
        std::fs::create_dir_all(&modules).expect("modules dir");
        let cfg = dir.path().join("node.toml");
        std::fs::write(
            &cfg,
            format!(
                "id = 1\nnamespace = \"t\"\npeer_seeds = [1]\nlisten = \"127.0.0.1:0\"\n\
                 storage_dir = {:?}\nmodules = {:?}\n",
                dir.path().join("storage").to_str().expect("utf8 path"),
                modules.to_str().expect("utf8 path")
            ),
        )
        .expect("write node.toml");
        let err = resolve(&cfg).expect_err("an empty bundle dir is refused");
        assert!(err.contains(&modules.display().to_string()), "{err}");
        assert!(err.contains("no module components"), "{err}");
    }

    /// a network with no modules is not a runnable network — its nodes would
    /// seed an empty code registry — so it is refused beside the empty
    /// validator set, by the loader BOTH paths run.
    #[test]
    fn network_shape_refuses_an_empty_module_list() {
        let dir = tmp("nomodules");
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        NetworkDescriptor {
            chain_id: "nomodules#12345678".into(),
            validators: vec![hex_bytes(me.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: Vec::new(),
            genesis: "ab".repeat(32),
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        std::fs::write(dir.join("node.toml"), network_shape_toml(&[])).expect("write node.toml");
        let err = resolve(&dir.join("node.toml")).expect_err("an empty module list is refused");
        assert!(err.contains("no modules"), "{err}");
    }

    #[test]
    fn dev_shape_duplicate_seeds_are_a_config_error() {
        let dir = tmp("devdups");
        std::fs::write(
            dir.join("node.toml"),
            format!(
                "id = 0\nlisten = \"127.0.0.1:52220\"\nnamespace = \"demo\"\n\
                 peer_seeds = [0, 1, 1]\n{UNREAD_BUNDLE}"
            ),
        )
        .expect("write");
        let err = resolve(&dir.join("node.toml")).expect_err("dup seeds refused");
        assert!(err.contains("duplicate seed"), "{err}");
    }

    #[test]
    fn the_announce_set_follows_the_compute_grant_and_nothing_else() {
        let dir = tmp("announce");
        // an EXPLICIT storage_dir: the dev shape otherwise defaults to a
        // shared `/tmp/ducktape-<id>`, and this test writes a grant into its
        // workspace — which would leak into every other test (and every later
        // run) that resolves the same id.
        let base = format!(
            "id = 0\nlisten = \"127.0.0.1:52221\"\nnamespace = \"demo\"\npeer_seeds = [0]\n\
             storage_dir = {:?}\n{}",
            dir.join("storage").to_str().expect("utf8 path"),
            fake_bundle(&dir)
        );
        let sandbox = sandbox_table("firecracker", 0, 0);

        // a sandbox table alone announces NOTHING: it says how a run would be
        // isolated, never that the user consented to run any.
        std::fs::write(dir.join("node.toml"), format!("{base}{sandbox}")).expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve ungranted");
        assert_eq!(resolved.compute_backend, None, "no grant, no compute plane");
        // the grant lives in the WORKSPACE, which for the dev shape is the
        // per-process state dir rather than the config dir.
        let workspace = resolved.service.workspace.clone();

        // a grant carrying tags is what opts this node into the pools.
        write_compute_grant(&workspace, &["quack-text", "quack-json"]);
        let resolved = resolve(&dir.join("node.toml")).expect("resolve granted");
        assert!(resolved.compute_backend.is_some());

        // a grant carrying NO tags is the accept-lane-only provider: the
        // compute plane runs, the announce stays empty. This is the only way
        // to express that state — there is no second switch.
        write_compute_grant(&workspace, &[]);
        let resolved = resolve(&dir.join("node.toml")).expect("resolve accept-lane-only");
        assert!(
            resolved.compute_backend.is_some(),
            "an empty grant still runs work, it just never advertises"
        );
    }

    /// write a `services.toml` granting compute with `tags` announced.
    fn write_compute_grant(workspace: &std::path::Path, tags: &[&str]) {
        std::fs::create_dir_all(workspace).expect("workspace dir");
        let list = tags
            .iter()
            .map(|tag| format!("\"{tag}\""))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            workspace.join("services.toml"),
            format!(
                "[[service]]\nkind = \"compute\"\ninstance = \"{}\"\n\
                 nonce = \"{}\"\ngranted_unix = 1700000000\ncapabilities = [{list}]\n\
                 scopes = []\n",
                "11".repeat(32),
                "22".repeat(16),
            ),
        )
        .expect("write services.toml");
    }

    /// one `[sandbox]` line-set for the dev-seed harness shape, appended
    /// LAST (everything after a toml table header belongs to the table).
    fn sandbox_table(runtime: &str, cores: u64, mem_gb: u64) -> String {
        format!("[sandbox]\nruntime = \"{runtime}\"\ncores = {cores}\nmem_gb = {mem_gb}\n")
    }

    #[test]
    fn sandbox_table_selects_the_compute_plane() {
        let dir = tmp("sandbox");
        let base = format!(
            "id = 0\nlisten = \"127.0.0.1:52222\"\nnamespace = \"demo\"\npeer_seeds = [0]\n{}",
            fake_bundle(&dir)
        );

        // no [sandbox] table ⇒ consensus-only: no backend, no capacity.
        std::fs::write(dir.join("node.toml"), &base).expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve default");
        assert_eq!(resolved.service.sandbox, None);
        assert!(
            resolved.service.sandbox_capacity.is_empty(),
            "a consensus-only node makes no capacity promise"
        );

        // the retired flat `sandbox = "direct"` spelling fails loudly — there
        // is no bare spawn to fall back to.
        std::fs::write(
            dir.join("node.toml"),
            format!("{base}sandbox = \"direct\"\n"),
        )
        .expect("write");
        resolve(&dir.join("node.toml")).expect_err("flat sandbox key refused");

        // firecracker ⇒ the workspace's two guest images and its executors
        // directory + probed capacity (0 = probe). The table names no path:
        // the images and the agent CLIs are the workspace's own files.
        std::fs::write(
            dir.join("node.toml"),
            format!("{base}{}", sandbox_table("firecracker", 0, 0)),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve firecracker");
        let workspace = resolved.service.workspace.clone();
        assert!(
            matches!(
                &resolved.service.sandbox,
                Some(SandboxBackend::MicroVm { vmm: Vmm::Firecracker, kernel, rootfs, executors })
                    if kernel == &workspace_config::guest_kernel(&workspace)
                        && rootfs == &workspace_config::guest_rootfs(&workspace)
                        && executors == &workspace_config::executor_dir(&workspace)
            ),
            "firecracker backend over the workspace's images: {:?}",
            resolved.service.sandbox
        );

        // the macOS runtime resolves the same shape with the vz flavor; the
        // OS gate lives in the boot probe, not in config resolution, so a
        // node.toml can be written on either OS and fail loudly on the wrong
        // one.
        std::fs::write(
            dir.join("node.toml"),
            format!("{base}{}", sandbox_table("vz", 0, 0)),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve vz");
        assert!(
            matches!(
                &resolved.service.sandbox,
                Some(SandboxBackend::MicroVm { vmm: Vmm::Vz, kernel, .. })
                    if kernel == &workspace_config::guest_kernel(&workspace)
            ),
            "vz backend over the workspace's images: {:?}",
            resolved.service.sandbox
        );
        assert!(
            resolved
                .service
                .sandbox_capacity
                .get("cores")
                .copied()
                .unwrap_or(0)
                >= 1,
            "a compute node announces its probed capacity"
        );

        // an override wins over the probe.
        std::fs::write(
            dir.join("node.toml"),
            format!("{base}{}", sandbox_table("firecracker", 99, 128)),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve overrides");
        assert_eq!(resolved.service.sandbox_capacity.get("cores"), Some(&99));
        assert_eq!(resolved.service.sandbox_capacity.get("mem_gb"), Some(&128));

        // any other runtime is a loud config error naming the one audited
        // adapter. "tart" and "podman" are in this list ON PURPOSE: both
        // backends were removed, and an operator whose node.toml still names
        // one must be told so at boot rather than silently getting something
        // else.
        for runtime in ["tart", "podman", "gvisor", "direct"] {
            std::fs::write(
                dir.join("node.toml"),
                format!("{base}{}", sandbox_table(runtime, 0, 0)),
            )
            .expect("write");
            let err = resolve(&dir.join("node.toml")).expect_err("unknown runtime refused");
            assert!(err.contains("firecracker"), "{err}");
        }
    }

    /// `primary_coordinator` (change 1, issue #331): the key ABSENT resolves
    /// to `None` — bit-identical to today, since `main.rs` re-derives the
    /// compiled default from `coordinator_ingress(None)` either way; the
    /// disable sentinel and an explicit override both ride the raw string
    /// through, unvalidated at resolve time (validated lazily at the point
    /// of use so a bad value degrades rather than aborting boot — inv 12).
    #[test]
    fn primary_coordinator_key_survives_resolve_default_absent_and_explicit() {
        let dir = tmp("primary-coordinator-key");
        let base = format!(
            "id = 0\nlisten = \"127.0.0.1:52260\"\nnamespace = \"demo\"\npeer_seeds = [0]\n{}",
            fake_bundle(&dir)
        );
        std::fs::write(dir.join("node.toml"), &base).expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve absent");
        assert_eq!(
            resolved.primary_coordinator, None,
            "absent key: re-derive the compiled default at the point of use"
        );

        std::fs::write(
            dir.join("node.toml"),
            format!("{base}primary_coordinator = \"none\"\n"),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve none");
        assert_eq!(resolved.primary_coordinator.as_deref(), Some("none"));
        assert_eq!(
            coordinator_ingress(resolved.primary_coordinator.as_deref()).expect("resolves"),
            None,
            "the persisted sentinel disables coordination on every future read"
        );

        std::fs::write(
            dir.join("node.toml"),
            format!("{base}primary_coordinator = \"203.0.113.9:3478\"\n"),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve override");
        assert_eq!(
            resolved.primary_coordinator.as_deref(),
            Some("203.0.113.9:3478")
        );
    }

    /// `coordinator_relay` rides resolve exactly like
    /// `primary_coordinator`: the key ABSENT resolves to `None` — the
    /// zero-config joiner default, deriving the relay from the ambient
    /// coordinator at the wiring site; the disable sentinel and an explicit
    /// override both ride the raw string through, unvalidated at resolve
    /// time (consumed lazily so a bad value degrades rather than aborting
    /// boot).
    #[test]
    fn coordinator_relay_key_survives_resolve_default_absent_and_explicit() {
        let dir = tmp("coordinator-relay-key");
        let base = format!(
            "id = 0\nlisten = \"127.0.0.1:52261\"\nnamespace = \"demo\"\npeer_seeds = [0]\n{}",
            fake_bundle(&dir)
        );
        std::fs::write(dir.join("node.toml"), &base).expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve absent");
        assert_eq!(
            resolved.coordinator_relay, None,
            "absent key: derive the relay from the ambient coordinator at the point of use"
        );

        for (value, expect) in [
            ("none", "none"),
            ("relay.example.com:8443", "relay.example.com:8443"),
        ] {
            std::fs::write(
                dir.join("node.toml"),
                format!("{base}coordinator_relay = \"{value}\"\n"),
            )
            .expect("write");
            let resolved = resolve(&dir.join("node.toml")).expect("resolve value");
            assert_eq!(resolved.coordinator_relay.as_deref(), Some(expect));
        }
    }

    /// `wireguard_advertised` (change 3, issue #331): the key ABSENT derives
    /// the endpoint the invite hands out — the dialable listen host at the
    /// WireGuard port; an explicit concrete override parses to a socket
    /// ingress, and a hostname stays a hostname (DNS deferred to plane start,
    /// same discipline as the mesh `advertised`).
    #[test]
    fn wireguard_advertised_key_absent_defaults_and_explicit_value_parses() {
        let dir = tmp("wg-advertised-key");
        let base = format!(
            "id = 0\nlisten = \"127.0.0.1:52270\"\nnamespace = \"demo\"\npeer_seeds = [0]\n\
             wireguard_listen = \"0.0.0.0:51820\"\n{}",
            fake_bundle(&dir)
        );
        std::fs::write(dir.join("node.toml"), &base).expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve absent");
        assert_eq!(
            resolved.wireguard_advertised,
            Some(Ingress::Socket("127.0.0.1:51820".parse().unwrap())),
            "absent key: the dialable listen host at the wireguard port — what the invite hands out"
        );

        std::fs::write(
            dir.join("node.toml"),
            format!("{base}wireguard_advertised = \"203.0.113.9:41820\"\n"),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve override");
        assert_eq!(
            resolved.wireguard_advertised,
            Some(Ingress::Socket("203.0.113.9:41820".parse().unwrap()))
        );

        std::fs::write(
            dir.join("node.toml"),
            format!(
                "{base}wireguard_advertised = \
                 \"definitely-not-resolvable.ducktape.invalid:41820\"\n"
            ),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("hostnames never block boot");
        assert!(
            matches!(
                &resolved.wireguard_advertised,
                Some(Ingress::Dns { port: 41820, .. })
            ),
            "wireguard_advertised stays a hostname: {:?}",
            resolved.wireguard_advertised
        );
    }

    /// a COMPLETE network-shape config (every key is required now), with
    /// per-key overrides for the aspect under test.
    fn network_shape_toml(overrides: &[(&str, &str)]) -> String {
        let defaults: &[(&str, &str)] = &[
            ("network", "\"network.toml\""),
            ("key_file", "\"identity.key\""),
            ("listen", "\"[::]:52320\""),
            ("advertised", "\"overlay\""),
            ("storage_dir", "'storage'"),
            ("http_listen", "\"127.0.0.1:0\""),
            ("gateway_listen", "\"127.0.0.1:0\""),
            ("rpc_listen", "\"127.0.0.1:0\""),
            ("wireguard_listen", "\"0.0.0.0:52323\""),
            ("invite_listen", "\"0.0.0.0:52324\""),
            ("wireguard_advertised", "\"auto\""),
            ("primary_coordinator", "\"none\""),
            ("coordinator_relay", "\"none\""),
            ("checkpoint_blocks", "32"),
        ];
        defaults
            .iter()
            .map(|(key, default)| {
                let value = overrides
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| *v)
                    .unwrap_or(default);
                format!("{key} = {value}\n")
            })
            .collect()
    }

    /// THE regression test for the daemon key hole: a service daemon must be
    /// able to resolve its node's config with `identity.key` ABSENT.
    ///
    /// This is what makes "the key is gone" checkable rather than asserted. It
    /// goes red the moment anything on `resolve_service`'s path calls
    /// `load_identity` again — no comment, no convention, a failing build.
    /// `resolve` on the SAME workspace is asserted to fail, so the test also
    /// proves the file is genuinely the node's only identity source and the
    /// daemon path simply does not need it.
    #[test]
    fn the_service_path_never_reads_the_node_key() {
        let dir = tmp("keyless-service");
        // a descriptor with somebody else's validator: this workspace has no
        // identity of its own on disk at all.
        let founder = ed25519::PrivateKey::from_seed(11).public_key();
        NetworkDescriptor {
            chain_id: "keyless#12345678".into(),
            validators: vec![hex_bytes(founder.as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        std::fs::write(dir.join("node.toml"), network_shape_toml(&[])).expect("write node.toml");
        assert!(
            !dir.join("identity.key").exists(),
            "the premise: this workspace holds no node key"
        );

        let service = resolve_service(&dir.join("node.toml"))
            .expect("the daemon path resolves with no node key on disk");
        assert_eq!(service.chain_id, "keyless#12345678");
        assert_eq!(service.storage_dir, dir.join("storage"));
        assert_eq!(service.workspace, dir);

        let node = resolve(&dir.join("node.toml"));
        assert!(
            node.is_err(),
            "the NODE path still requires the key it signs with"
        );
    }

    // NOTE: the anti-drift pin `the_service_view_agrees_with_the_node_view` was
    // DELETED here, not weakened. It compared the six facts field by field
    // across two parallel shapes; `Resolved` now CONTAINS the `ServiceConfig` it
    // used to duplicate, so the comparison it made is `x == x` — a tautology no
    // change to this file can falsify. Drift is not tested for because it can no
    // longer be written down.
    //
    // The refusal set below is the one thing the nesting does NOT make
    // structural — a `Result` has no field to share — so it keeps a real test.

    /// A descriptor no node will boot on must not be one a DAEMON signals
    /// against.
    ///
    /// The service path loads `network.toml` for its chain id, so it would
    /// happily resolve a descriptor with an empty validator set — and
    /// `ducktape service run compute` would then announce capacity for a
    /// network its own node refuses to start. Both paths run
    /// [`load_valid_descriptor`], and this is what says so: weaken either
    /// refusal and one of these assertions goes red.
    #[test]
    fn both_paths_refuse_a_descriptor_no_node_can_run() {
        let (slug, expected) = ("novalidators", "no validators");
        let dir = tmp(slug);
        // a REAL key on disk, so the node path fails on the descriptor
        // rather than on a missing identity.
        load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        NetworkDescriptor {
            chain_id: format!("{slug}#12345678"),
            validators: vec![],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        }
        .save(&dir.join("network.toml"))
        .expect("save descriptor");
        std::fs::write(dir.join("node.toml"), network_shape_toml(&[])).expect("write");

        let node = resolve(&dir.join("node.toml")).expect_err("the node path refuses it");
        assert!(node.contains(expected), "node path: {node}");
        let service = resolve_service(&dir.join("node.toml")).expect_err("so must the daemon path");
        assert!(service.contains(expected), "service path: {service}");
    }

    /// the desktop shape's posture: a config with no dialable underlay host
    /// (advertised = "overlay", unspecified binds) mints no direct intro
    /// endpoint (`cmd_invite`), so the resolved DIRECT intro listener is
    /// None — the plane binds no wildcard UDP socket (a macOS firewall
    /// prompt trigger) and joins ride the coordinated path. any mintable
    /// host keeps the listener.
    #[test]
    fn intro_listener_resolves_only_when_a_direct_endpoint_is_mintable() {
        let dir = tmp("intro-listener");
        let (me, _) = load_or_generate_identity(&dir.join("identity.key")).expect("keygen");
        let d = NetworkDescriptor {
            chain_id: "net#55555555".into(),
            validators: vec![hex_bytes(me.public_key().as_ref())],
            bootstrap: vec![],
            reach: vec![],
            coordination: None,
            block_time_ms: DEFAULT_BLOCK_TIME_MS,
            modules: fake_modules(),
            genesis: "ab".repeat(32),
        };
        d.save(&dir.join("network.toml")).expect("save");

        // the desktop shape: overlay-advertised, unspecified binds.
        std::fs::write(dir.join("node.toml"), network_shape_toml(&[])).expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve desktop shape");
        assert_eq!(
            r.invite_listen, None,
            "no dialable underlay host → no direct intro listener"
        );

        // the port-forwarded NAT shape: wireguard_advertised keeps it.
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[("wireguard_advertised", "\"203.0.113.9:41820\"")]),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve port-forwarded shape");
        assert_eq!(
            r.invite_listen,
            Some("0.0.0.0:52324".parse().unwrap()),
            "a dialable WG endpoint keeps the direct intro listener"
        );

        // the server shape: a concrete advertised keeps it.
        std::fs::write(
            dir.join("node.toml"),
            network_shape_toml(&[("advertised", "\"203.0.113.9:52320\"")]),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve server shape");
        assert_eq!(
            r.invite_listen,
            Some("0.0.0.0:52324".parse().unwrap()),
            "a dialable advertised keeps the direct intro listener"
        );
    }

    /// the endpoint a peer learns from this node's signed mesh record is the
    /// one the invite blob hands out — ONE derivation. the real-network lane
    /// found the plane deriving from `wireguard_listen` alone: two joiners on
    /// one LAN, both bound `0.0.0.0`, both advertised no endpoint, and
    /// neither could ever initiate the other's tunnel.
    #[test]
    fn wireguard_endpoint_derives_from_advertised_when_the_bind_is_unspecified() {
        let dir = tmp("wg-advertised-derived");
        let bundle = fake_bundle(&dir);
        std::fs::write(
            dir.join("node.toml"),
            format!(
                "id = 0\nlisten = \"0.0.0.0:52272\"\nnamespace = \"demo\"\npeer_seeds = [0]\n\
                 advertised = \"192.0.2.7:52272\"\nwireguard_listen = \"0.0.0.0:51820\"\n{bundle}"
            ),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve advertised");
        assert_eq!(
            resolved.wireguard_advertised,
            Some(Ingress::Socket("192.0.2.7:51820".parse().unwrap())),
            "the advertised host at the wireguard port"
        );

        // no dialable underlay host at all (the desktop shape): endpoint-less,
        // and that is a shape, never an error.
        std::fs::write(
            dir.join("node.toml"),
            format!(
                "id = 0\nlisten = \"[::]:52272\"\nnamespace = \"demo\"\npeer_seeds = [0]\n\
                 advertised = \"overlay\"\nwireguard_listen = \"[::]:51820\"\n{bundle}"
            ),
        )
        .expect("write");
        let resolved = resolve(&dir.join("node.toml")).expect("resolve overlay");
        assert_eq!(resolved.wireguard_advertised, None);
    }

    #[test]
    fn wireguard_advertised_rejects_an_unspecified_or_port_zero_value() {
        let dir = tmp("wg-advertised-bad");
        std::fs::write(
            dir.join("node.toml"),
            format!(
                "id = 0\nlisten = \"127.0.0.1:52280\"\nnamespace = \"demo\"\npeer_seeds = [0]\n\
                 wireguard_listen = \"0.0.0.0:51820\"\n\
                 wireguard_advertised = \"0.0.0.0:0\"\n{UNREAD_BUNDLE}"
            ),
        )
        .expect("write");
        let err = resolve(&dir.join("node.toml")).expect_err(
            "an explicit unspecified/port0 value is a config error, not a silent \
                          fallback to endpoint-less",
        );
        assert!(err.contains("wireguard_advertised"), "{err}");
    }

    /// `endpoint_host` (change 3's invite-mint analog, config.rs ~2068): an
    /// explicit `wireguard_advertised` wins outright, including over a
    /// concrete `wireguard_listen` IP; absent falls back to today's
    /// derivation exactly (wireguard_listen IP, else `advertised`/`listen`).
    #[test]
    fn endpoint_host_prefers_wireguard_advertised_over_the_listen_derivation() {
        let unspecified: SocketAddr = "0.0.0.0:51820".parse().unwrap();
        let concrete: SocketAddr = "10.0.0.5:51820".parse().unwrap();

        assert_eq!(
            endpoint_host(None, "127.0.0.1:0", concrete, Some("203.0.113.9:9999"))
                .unwrap()
                .as_deref(),
            Some("203.0.113.9"),
            "wireguard_advertised wins even over a concrete wireguard_listen IP"
        );
        assert_eq!(
            endpoint_host(
                None,
                "127.0.0.1:0",
                unspecified,
                Some("tunnel.example.com:9999")
            )
            .unwrap()
            .as_deref(),
            Some("tunnel.example.com"),
            "a hostname override stays a hostname"
        );
        assert_eq!(
            endpoint_host(None, "127.0.0.1:0", concrete, None)
                .unwrap()
                .as_deref(),
            Some("10.0.0.5"),
            "absent: today's derivation — the concrete wireguard_listen IP wins"
        );
        assert_eq!(
            endpoint_host(Some("203.0.113.1:443"), "127.0.0.1:0", unspecified, None)
                .unwrap()
                .as_deref(),
            Some("203.0.113.1"),
            "absent + unspecified listen: falls back to `advertised`/`listen`, unchanged"
        );
    }

    /// A config that NAMES no dialable host is endpoint-less, not broken: the
    /// single-box founder shape (`advertised = "overlay"`, unspecified binds)
    /// answers `Ok(None)` so its `invite` keeps minting. A config that is
    /// WRONG still refuses — absence and invalidity are different answers, and
    /// collapsing them onto the error channel is what made a founder's own
    /// invite read as a failure.
    #[test]
    fn a_config_that_names_no_host_is_endpoint_less_not_a_refusal() {
        let unspecified: SocketAddr = "0.0.0.0:51820".parse().unwrap();

        assert_eq!(
            endpoint_host(Some("overlay"), "[::]:8846", unspecified, None).unwrap(),
            None,
            "the overlay shape names no underlay host — endpoint-less, not an error"
        );
        assert_eq!(
            invite_wireguard_endpoint(Some("overlay"), "[::]:8846", unspecified, None).unwrap(),
            None,
            "and the minted endpoint carries that answer through"
        );
        let err = endpoint_host(Some("0.0.0.0:8846"), "[::]:8846", unspecified, None)
            .expect_err("an advertised that can never be dialed is still a config error");
        assert!(err.contains("not dialable"), "{err}");
    }

    /// The invite-mint endpoint (review fix, change 3): with
    /// `wireguard_advertised` set the minted `endpoint` is the advertised
    /// value VERBATIM — host AND port. The port-forwarded scenario the key
    /// exists for: external 41820 forwarded to bind 51820 — baking the
    /// advertised host with the BIND port would silently mint a wrong
    /// endpoint. Absent, the derivation is bit-identical to before: the
    /// `endpoint_host` host at the bind port.
    #[test]
    fn invite_endpoint_uses_the_advertised_value_verbatim_including_its_port() {
        let unspecified: SocketAddr = "0.0.0.0:51820".parse().unwrap();
        let concrete: SocketAddr = "10.0.0.5:51820".parse().unwrap();

        assert_eq!(
            invite_wireguard_endpoint(None, "127.0.0.1:0", unspecified, Some("203.0.113.9:41820"))
                .unwrap()
                .as_deref(),
            Some("203.0.113.9:41820"),
            "the advertised endpoint rides verbatim — 41820, never the bind port 51820"
        );
        assert_eq!(
            invite_wireguard_endpoint(
                None,
                "127.0.0.1:0",
                concrete,
                Some("tunnel.example.com:41820")
            )
            .unwrap()
            .as_deref(),
            Some("tunnel.example.com:41820"),
            "a hostname override stays a hostname, with ITS port — even over a concrete bind IP"
        );
        assert_eq!(
            invite_wireguard_endpoint(None, "127.0.0.1:0", concrete, None)
                .unwrap()
                .as_deref(),
            Some("10.0.0.5:51820"),
            "absent: today's derivation exactly — the wireguard_listen IP at the bind port"
        );
        assert_eq!(
            invite_wireguard_endpoint(Some("203.0.113.1:443"), "127.0.0.1:0", unspecified, None)
                .unwrap()
                .as_deref(),
            Some("203.0.113.1:51820"),
            "absent + unspecified listen: the advertised HOST at the WG bind port, unchanged"
        );
    }

    /// THE invariant behind the mint refusal, over the whole resolution
    /// space rather than the one shape that caught it: a front a stranger
    /// gets handed is loopback ONLY IF the operator wrote a loopback address
    /// down themselves.
    ///
    /// Both halves matter and neither implies the other. A front the node
    /// DERIVED (`wireguard_advertised = "auto"` reusing the p2p hint) that
    /// lands on loopback must be refused — that is issue #2732, where a
    /// founder handed a stranger `127.0.0.1:<port>` and nothing said so until
    /// the redemption failed. A front the operator NAMED must still mint,
    /// loopback or not: a same-box joiner is a real shape the e2e suites run.
    ///
    /// Every combination of what the two tunnel keys can say, against a p2p
    /// hint that is loopback (the `node init` single-box default, and the
    /// case that produced the bug), routable, and absent.
    #[test]
    fn a_minted_front_is_loopback_only_if_the_operator_named_loopback() {
        // `node init` writes the sentinel when no flag is passed, and an
        // ABSENT key defaults to it: both mean "the operator named nothing"
        // by the time the resolution sees them.
        fn configured(v: Option<&str>) -> Option<&str> {
            v.filter(|v| *v != "auto")
        }
        let is_loopback =
            |host: &str| host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());

        let advertised_values = [None, Some("auto"), Some("127.0.0.1:41820"), Some("203.0.113.9:41820")];
        let listen_values = ["0.0.0.0:51820", "127.0.0.1:51820", "192.0.2.7:51820"];
        let p2p_values = [Some("127.0.0.1:52330"), Some("203.0.113.1:443"), Some("overlay")];

        for wireguard_advertised in advertised_values {
            for wireguard_listen in listen_values {
                for p2p in p2p_values {
                    let wg: SocketAddr = wireguard_listen.parse().unwrap();
                    let case =
                        format!("advertised={wireguard_advertised:?} listen={wireguard_listen} p2p={p2p:?}");
                    let Some(front) = invite_front(
                        p2p,
                        "[::]:52330",
                        wg,
                        configured(wireguard_advertised),
                    )
                    .unwrap_or_else(|e| panic!("{case}: {e}")) else {
                        // no address in this config at all — an endpoint-less
                        // blob, which hands nobody a loopback front.
                        continue;
                    };

                    // the two ways an operator names a tunnel address, and
                    // the only two that license a loopback front.
                    let named_a_loopback_front = configured(wireguard_advertised)
                        .is_some_and(|a| is_loopback(a.rsplit_once(':').unwrap().0))
                        || wg.ip().is_loopback();
                    let mints = !front.is_derived_loopback();

                    if mints && is_loopback(&front.host) {
                        assert!(
                            named_a_loopback_front,
                            "{case}: minted the loopback front {} nobody named",
                            front.host
                        );
                    }
                    assert!(
                        !named_a_loopback_front || mints,
                        "{case}: refused a loopback front the operator wrote down"
                    );
                    // the host and the full endpoint are one decision: they
                    // can never name different machines.
                    assert!(
                        front.endpoint.starts_with(&front.host),
                        "{case}: endpoint {} left the host {} behind",
                        front.endpoint,
                        front.host
                    );
                }
            }
        }

        // the cell the issue was filed from, spelled out: nothing named, an
        // unspecified bind, a loopback p2p hint.
        let refused = invite_front(
            Some("127.0.0.1:52330"),
            "[::]:52330",
            "0.0.0.0:51820".parse().unwrap(),
            None,
        )
        .unwrap()
        .expect("the p2p hint is a front");
        assert_eq!(refused.source, FrontSource::Derived);
        assert!(refused.is_derived_loopback(), "{refused:?}");

        // and the same shape once the operator answers it.
        let named = invite_front(
            Some("127.0.0.1:52330"),
            "[::]:52330",
            "0.0.0.0:51820".parse().unwrap(),
            Some("192.168.0.151:46900"),
        )
        .unwrap()
        .expect("a named front is a front");
        assert_eq!(named.source, FrontSource::Named);
        assert_eq!(named.endpoint, "192.168.0.151:46900");
    }

    #[test]
    fn dev_shape_never_bootstraps_itself() {
        let dir = tmp("devself");
        std::fs::write(
            dir.join("node.toml"),
            format!(
                "id = 1\nlisten = \"127.0.0.1:52230\"\nnamespace = \"demo\"\npeer_seeds = [1, 0]\n\
                 peer_addrs = [\"127.0.0.1:52230\", \"127.0.0.1:52231\"]\n{}",
                fake_bundle(&dir)
            ),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve");
        assert_eq!(
            r.dial_hints.len(),
            1,
            "self is filtered, the other peer stays"
        );
        assert_eq!(
            r.dial_hints[0].0,
            ed25519::PrivateKey::from_seed(0).public_key(),
            "the surviving hint is the OTHER seed"
        );
    }

    #[test]
    fn dev_shape_builds_the_full_hint_list() {
        let dir = tmp("dev");
        let toml = format!(
            r#"
id = 1
listen = "127.0.0.1:52210"
namespace = "demo"
peer_seeds = [0, 1, 2]
validator_seeds = [0, 1]
peer_addrs = ["127.0.0.1:52200", "127.0.0.1:52210", "127.0.0.1:52202"]
{}"#,
            fake_bundle(&dir)
        );
        std::fs::write(dir.join("node.toml"), toml).expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve");
        assert!(r.dev_demo);
        assert_eq!(r.label, "#1");
        assert_eq!(r.mesh.len(), 3);
        assert_eq!(r.validators.len(), 2);
        assert_eq!(r.dial_hints.len(), 2, "every peer but self carries a hint");
        assert_eq!(
            r.dial_hints[0].0,
            ed25519::PrivateKey::from_seed(0).public_key(),
            "hints keep peer_seeds order"
        );
        assert_eq!(
            r.signer.public_key(),
            ed25519::PrivateKey::from_seed(1).public_key()
        );
    }

    /// the retired `wireguard_effect` key is GONE, not tolerated: any
    /// spelling fails the strict parse in both shapes.
    #[test]
    fn retired_wireguard_effect_key_is_refused() {
        let dir = tmp("wgeffect");
        let base = format!(
            "id = 0\nlisten = \"127.0.0.1:52230\"\nnamespace = \"demo\"\npeer_seeds = [0]\n\
             {UNREAD_BUNDLE}"
        );
        for spelled in ["socket", "fake", "tun"] {
            std::fs::write(
                dir.join("node.toml"),
                format!("{base}wireguard_effect = \"{spelled}\"\n"),
            )
            .expect("write");
            let err = resolve(&dir.join("node.toml")).expect_err("retired key refused");
            assert!(err.contains("wireguard_effect"), "{spelled}: {err}");
        }
    }

    #[test]
    fn overlay_advertised_derives_the_ula_and_requires_v6_listen() {
        let dir = tmp("overlay-advertised");
        // a REAL bundle: this test's first half resolves successfully, so it
        // reaches the hashing the refusal half never gets to.
        let base = format!(
            "id = 1\nnamespace = \"demo\"\npeer_seeds = [0, 1]\n\
             peer_addrs = [\"127.0.0.1:52240\", \"127.0.0.1:52241\"]\n{}",
            fake_bundle(&dir)
        );
        std::fs::write(
            dir.join("node.toml"),
            format!("{base}listen = \"[::]:52241\"\nadvertised = \"overlay\"\n"),
        )
        .expect("write");
        let r = resolve(&dir.join("node.toml")).expect("resolve");
        let identity = wireguard::ValidatorIdentity::try_from(
            ed25519::PrivateKey::from_seed(1).public_key().as_ref(),
        )
        .unwrap();
        let ula = wireguard::ula_v6_member_addr("demo", identity);
        assert_eq!(
            r.advertised,
            Ingress::Socket(SocketAddr::new(std::net::IpAddr::V6(ula), 52241)),
            "the overlay sentinel advertises the chain-derived ULA at the listen port"
        );

        // the overlay is v6: a v4-only listener would never see tunnel SYNs.
        std::fs::write(
            dir.join("node.toml"),
            format!("{base}listen = \"0.0.0.0:52241\"\nadvertised = \"overlay\"\n"),
        )
        .expect("write");
        let err = resolve(&dir.join("node.toml")).expect_err("v4 listener refused");
        assert!(err.contains("IPv6"), "{err}");
    }
}
