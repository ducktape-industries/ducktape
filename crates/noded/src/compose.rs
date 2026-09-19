//! The module composer: deployment hashes, a code source, and a store source
//! produce a [`Host`]. Genesis, checkpoint restore, state sync, and live
//! admissions all instantiate Wasm through [`wasm_module`]. Each component's
//! declared shape chooses its backing, configuration keys, and query mode.
//!
//! Genesis supplies deployment hashes and initialization parameters. Reopen
//! supplies the checkpoint's authenticated deployment hashes, including the
//! registry's own code. The reopened registry identifies later admissions and
//! the code designated for each replay height. No native module is inserted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use host::Host;
use sdk::{MerkleStore, Module, StateRoot};
use sha2::Digest as _;
use wasm_host::{Backing, CompiledModule, Shape, WasmModule};

mod view_abi {
    macro_rules! bindings {
        ($wit:literal) => {
            wasmtime::component::bindgen!({ inline: $wit, world: "view" });
        };
    }
    view_wire::with_view_wit!(bindings);
}

/// a boxed, non-`Send` future (the host and every store are `!Send`).
pub type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// the store source: a module id → its opened (or synced) authenticated store.
/// the caller decides the lifecycle; a refused open is an `Err` naming why.
pub type StoreSource<'a> = dyn FnMut(&str) -> BoxFut<'a, Result<Box<dyn MerkleStore>, String>> + 'a;

/// the snapshot source a [`Start::Resume`] installs from: a module id and its
/// declared backing → the `(bytes, root)` to install, or `None` when the
/// module's state at the boundary lives in its store or on its disk already.
/// never asked for a store-backed module (its state IS its store).
pub type SnapshotSource<'a> =
    dyn FnMut(&str, Backing) -> BoxFut<'a, Result<Option<(Vec<u8>, StateRoot)>, String>> + 'a;

/// Storage locations are node configuration, while the guest declares its
/// engine. An unbound module gets a private directory without a native ID list.
#[derive(Clone)]
pub struct Substrates {
    pub directory: PathBuf,
    pub bindings: std::collections::BTreeMap<String, PathBuf>,
    pub blobs: blobstore::BlobHandle,
}

impl Substrates {
    pub fn path(&self, id: &str) -> Result<PathBuf, String> {
        workspace_config::validate_module_id(id)?;
        Ok(self
            .bindings
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.directory.join(id)))
    }
}

/// the per-network values every composition binds into module state: the
/// invite namespace governance verifies tokens and join proofs against, the
/// identity chain id identity/gateway/runs scope their records to, and the
/// unit this network's `consensus_time` advances in. a network-bound module's
/// `__config` record is made of these. needed on EVERY path, not just
/// genesis: a module admitted after a checkpoint starts fresh at restore and
/// seeds its config then, exactly as it did live.
pub struct Bindings<'a> {
    pub invite: &'a [u8],
    pub chain_id: &'a str,
    /// what one `consensus_time` unit IS here — height on the validator and
    /// replica lanes, milliseconds on the sim lane. A module that turns a
    /// duration into a deadline reads it; one that does not, ignores it.
    pub time_unit: sdk::genesis_config::TimeUnit,
}

/// how the composed host comes up.
pub enum Boot<'a, 'b> {
    /// Block zero: `bundle` maps ids to deployment hashes. Every component
    /// starts fresh and receives the same encoded initialization parameters;
    /// the registry and validator set consume their respective entries.
    Genesis {
        validators: &'a [Vec<u8>],
        bundle: &'a BTreeMap<String, [u8; 32]>,
    },
    /// A checkpoint or state-sync boundary at `height`. `codes` authenticates
    /// the running deployments independently of the registry they implement.
    /// Stores reopen and map/ODB tenants install their boundary snapshots.
    Reopen {
        height: u64,
        codes: &'a BTreeMap<String, [u8; 32]>,
        /// two lifetimes, like `StoreSource`'s `&mut StoreSource<'_>`: the
        /// borrow ends with the compose, the futures' lifetime is the
        /// closure's, so one source serves a compose AND a later
        /// [`wasm_module`].
        snapshots: &'a mut SnapshotSource<'b>,
    },
}

/// how ONE wasm module comes up.
pub enum Start<'a, 'b> {
    /// no state yet — a genesis tenant, a live admission, or a reopen before
    /// the module's first activation: its `__config` record seeds from the
    /// bindings (a store-backed module commits it into its merkle store, a
    /// map-backed one installs it as its initial map; an odb-backed one
    /// carries it alongside the wrap in [`wasm_module`] regardless of `start`,
    /// since it is never persisted — see [`wasm_host::CompiledModule::over_odb`]).
    Fresh { parameters: &'a [u8] },
    /// the module's state at a boundary: its store reopens or resyncs (the
    /// store source's business) and `snapshots(id, backing)` installs a
    /// map/odb image if the source has one.
    Resume {
        snapshots: &'a mut SnapshotSource<'b>,
    },
}

/// one founding deployment, fetched and verified: what the registry seeds
/// for it (`kind`) is what its frame says it is.
pub struct Founding {
    pub id: String,
    pub hash: [u8; 32],
    pub kind: modules::Kind,
    /// what the frame declares it needs on the data plane. Read off the same
    /// bytes the hash covers, so genesis is not a second declarer.
    pub lanes: Vec<modules::LaneDecl>,
}

/// what a deployment frame IS, by its tag: the kind the registry seeds a
/// genesis entry with, and the kind a post-genesis registration must match.
pub fn artifact_kind(bytes: &[u8]) -> Result<modules::Kind, String> {
    Ok(match module_artifact::ArtifactRef::decode(bytes)? {
        module_artifact::ArtifactRef::Module(_) => modules::Kind::Module,
        module_artifact::ArtifactRef::View(_) => modules::Kind::View,
    })
}

/// the data-plane lanes a deployment frame declares — what the registry
/// admits for it, whether the frame arrives at genesis or through a
/// governance admission. A view frame declares none: it has no consensus code
/// and no sockets, so it has nothing to speak on a lane with.
pub fn artifact_lanes(bytes: &[u8]) -> Result<Vec<modules::LaneDecl>, String> {
    Ok(match module_artifact::ArtifactRef::decode(bytes)? {
        module_artifact::ArtifactRef::Module(module) => module.lanes,
        module_artifact::ArtifactRef::View(_) => Vec::new(),
    })
}

/// the registry's genesis seed table for the founding set: every entry's
/// kind is its frame's tag — a `<id>.component.wasm` founds a module, a
/// `<id>.view.wasm` alone founds a view (`workspace_config::Genesis::compose`).
pub fn genesis_seeds(founding: &[Founding]) -> BTreeMap<String, modules::Seed> {
    founding
        .iter()
        .map(|entry| {
            let seed = modules::Seed {
                kind: entry.kind,
                code_hash: entry.hash.to_vec(),
                lanes: entry.lanes.clone(),
            };
            (entry.id.clone(), seed)
        })
        .collect()
}

/// Compose the boot mode's deployment set into a [`Host`];
/// the boot mode supplies the authenticated module set and initialization or
/// snapshot data. Every module uses the same Wasm constructor.
pub async fn compose(
    code: &dyn host::CodeSource,
    stores: &mut StoreSource<'_>,
    substrates: &Substrates,
    bindings: &Bindings<'_>,
    mut boot: Boot<'_, '_>,
) -> Result<Host, String> {
    let mut host = Host::new();
    let codes = match &boot {
        Boot::Genesis { bundle, .. } => *bundle,
        Boot::Reopen { codes, .. } => *codes,
    };
    for id in codes.keys() {
        workspace_config::validate_module_id(id)?;
    }
    // every deployment is fetched and verified before anything seats: the
    // genesis seed table names each entry's kind off its frame, and a
    // reopen's set is the seated modules' hashes (a checkpoint records what
    // ran, and a view never runs), so a view frame can only be a founding one.
    let mut founding = Vec::with_capacity(codes.len());
    let mut fetched = Vec::with_capacity(codes.len());
    for (id, hash) in codes {
        let bytes = fetch_code(code, id, hash).await?;
        founding.push(Founding {
            id: id.clone(),
            hash: *hash,
            kind: artifact_kind(&bytes)?,
            lanes: artifact_lanes(&bytes)?,
        });
        fetched.push((id, bytes));
    }
    let parameters = match &boot {
        Boot::Genesis { validators, .. } => sdk::genesis_config::encode_config(&[
            ("modules", &sdk::wire::encode(&genesis_seeds(&founding))),
            ("validators", &sdk::wire::encode(validators)),
        ]),
        Boot::Reopen { .. } => sdk::genesis_config::encode_config(&[]),
    };
    for (entry, (id, bytes)) in founding.iter().zip(fetched) {
        let start = match &mut boot {
            Boot::Genesis { .. } => Start::Fresh {
                parameters: &parameters,
            },
            Boot::Reopen { snapshots, .. } => Start::Resume {
                snapshots: &mut **snapshots,
            },
        };
        match entry.kind {
            modules::Kind::Module => {
                let module = wasm_module(id, &bytes, stores, substrates, bindings, start).await?;
                register_new(&mut host, Box::new(module))?;
            }
            // a view seats nothing: the registry entry carries its hash and
            // the desktop fetches the artifact by that hash. a plane seats
            // nothing here either — its artifact is realized off the module
            // boundary by the node plane that owns it.
            modules::Kind::View | modules::Kind::Plane => {}
        }
    }
    // Durable stores can have advanced beyond the checkpoint. Its registry
    // names admissions replay will encounter; prepare those through the same
    // fresh-state path used when they were admitted live.
    if let Boot::Reopen {
        height, snapshots, ..
    } = &mut boot
    {
        for entry in registry_active_set(&host, *height).await? {
            if host.module_root(&entry.id).is_some() {
                continue;
            }
            let bytes = fetch_code(code, &entry.id, &entry.hash).await?;
            let start = match entry.seat {
                Seat::Fresh => Start::Fresh {
                    parameters: &parameters,
                },
                Seat::Resume => Start::Resume {
                    snapshots: &mut **snapshots,
                },
            };
            let module =
                wasm_module(&entry.id, &bytes, stores, substrates, bindings, start).await?;
            register_new(&mut host, Box::new(module))?;
        }
    }
    Ok(host)
}

/// register a module the host does not hold yet. dispatch addresses modules
/// by id, so a second module under one id — a registry roster entry colliding
/// with another entry — is refused, never silently replaced.
fn register_new(host: &mut Host, module: Box<dyn Module>) -> Result<(), String> {
    let id = module.id();
    if host.module_root(&id).is_some() {
        return Err(format!("duplicate module id: {id}"));
    }
    host.register(module);
    Ok(())
}

/// one entry of the wasm set: the code `id` runs, and how it starts.
struct ActiveCode {
    id: String,
    hash: [u8; 32],
    seat: Seat,
}

/// how a module of the registry's roster starts at a boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seat {
    /// the boundary predates the module's first activation: nothing to
    /// resume, it starts as it did live.
    Fresh,
    /// the module was live at the boundary, which holds its state.
    Resume,
}

/// the code `entry`'s module runs at the end of block `height` and how it
/// starts there: the registry's designated code (`modules::code_at` — a
/// pending swap armed at `height` wins, else the latest activation at or
/// before it), fresh when the module's first activation is past `height`,
/// resumed otherwise. `None` for a module registered but never activated:
/// nothing to run.
pub fn seat_at(entry: &modules::ModuleCode, height: u64) -> Option<([u8; 32], Seat)> {
    let hash: [u8; 32] = modules::code_at(entry, height)?.try_into().ok()?;
    let first_activation = entry.history.first().map(|a| a.height);
    let activated_by_height = first_activation.is_some_and(|activated| activated <= height);
    let seat = if activated_by_height {
        Seat::Resume
    } else {
        Seat::Fresh
    };
    Some((hash, seat))
}

/// the modules registry's roster at `height` ([`seat_at`] per entry). the
/// registry is optional: without one there are no later admissions to seat.
/// a registry that IS there and fails to answer is an error, never an empty
/// roster — seating nothing would silently drop every admitted module.
async fn registry_active_set(host: &Host, height: u64) -> Result<Vec<ActiveCode>, String> {
    let status = host
        .module_status()
        .await
        .map_err(|e| format!("modules registry query failed: {e}"))?;
    let Some(roster) = status else {
        return Ok(Vec::new());
    };
    Ok(roster
        .into_iter()
        .filter(|entry| match entry.kind {
            modules::Kind::Module => true,
            // a view is a registry entry with nothing to seat, and so is a
            // plane — the node plane that owns it realizes its artifact.
            modules::Kind::View | modules::Kind::Plane => false,
        })
        .filter_map(|entry| {
            let (hash, seat) = seat_at(&entry, height)?;
            Some(ActiveCode {
                id: entry.module_id,
                hash,
                seat,
            })
        })
        .collect())
}

/// the component bytes for `id` at `hash`, verified. a code source is a
/// lookup, not a guarantee: the bytes are re-hashed here exactly as the
/// host's swap path re-checks them, so a lying source (a dir keyed by
/// filename, a stale blob) can never seat code whose `code_hash()` disagrees
/// with the registry entry.
pub async fn fetch_code(
    code: &dyn host::CodeSource,
    id: &str,
    hash: &[u8; 32],
) -> Result<Vec<u8>, String> {
    let bytes = code.fetch(hash).await.ok_or_else(|| {
        format!(
            "code bytes absent for module {id} (hash {}) — fail-closed",
            crate::hex_bytes(hash)
        )
    })?;
    let matches_hash = sha2::Sha256::digest(&bytes)[..] == hash[..];
    if !matches_hash {
        return Err(format!(
            "module {id} code bytes do not match hash {} — fail-closed",
            crate::hex_bytes(hash)
        ));
    }
    Ok(bytes)
}

/// can THIS build load `id`'s deployment at all? `compile_artifact`
/// instantiates the bytes against this binary's real `ducktape:module/host`
/// world, so a component whose imports that world no longer provides is
/// refused right here — which is the ONLY refusal the whole wasm path can
/// reach without a store, a substrate or a byte of state. that is what makes
/// it askable of a STAGED binary beside a running node: the roster comes off
/// the node's rpc and the bytes off the blobstore's immutable files, and the
/// sentence a refusal carries is the very one the next boot would print.
pub fn load(id: &str, bytes: &[u8]) -> Result<CompiledModule, String> {
    workspace_config::validate_module_id(id)?;
    let compiled = CompiledModule::compile_artifact(bytes)
        .map_err(|e| format!("{id} component loads: {e}"))?;
    check_realizable(id, compiled.shape())?;
    Ok(compiled)
}

/// the ONE wasm path: wrap `bytes` for `id` over the substrate its declared
/// shape names. a store-backed module opens its store through the source; an
/// odb-backed one opens the host substrate for its id and carries its
/// `__config` alongside the wrapping call (never installed — see
/// [`wasm_host::CompiledModule::over_odb`]); a map-backed one starts from an
/// empty map. then the start: fresh seeds a MAP tenant's `__config` record, a
/// resume installs the boundary snapshot the source offers.
pub async fn wasm_module(
    id: &str,
    bytes: &[u8],
    stores: &mut StoreSource<'_>,
    substrates: &Substrates,
    bindings: &Bindings<'_>,
    start: Start<'_, '_>,
) -> Result<WasmModule, String> {
    let compiled = load(id, bytes)?;
    let shape = compiled.shape().clone();
    let is_fresh = matches!(start, Start::Fresh { .. });
    let wrapped = match shape.backing {
        Backing::Map => compiled.over_map(id),
        Backing::Store => {
            let mut store = stores(id).await?;
            if is_fresh {
                seed_store_config(&mut *store, id, &shape, bindings).await?;
            }
            compiled.over_store(id, store)
        }
        Backing::Odb | Backing::Git => {
            let backing = open_odb(id, shape.backing, substrates)?;
            let config = odb_genesis_config(id, &shape, bindings)?;
            compiled.over_odb(id, backing, config)
        }
    };
    let mut module = wrapped.map_err(|e| format!("{id} component loads: {e}"))?;
    match start {
        // a MAP-backed network-bound module carries its `__config` in its
        // state map, so a fresh start INSTALLS the record the store-backed
        // twin committed above. it then rides snapshots and state-sync like
        // any other map entry (the guest's `save_state` never touches that
        // key), so only a fresh start seeds it — a resume's install below
        // replaces the whole map, config included.
        Start::Fresh { parameters } => {
            let seeds_map_config = shape.backing == Backing::Map && !shape.config.is_empty();
            if seeds_map_config {
                let (bytes, root) = wasm_host::initial_state(&[(
                    sdk::genesis_config::CONFIG_KEY,
                    &encode_config(id, &shape, bindings)?,
                )]);
                module
                    .install(&bytes, root)
                    .map_err(|e| format!("{id} genesis config installs: {e}"))?;
            }
            module
                .initialize(parameters)
                .await
                .map_err(|e| format!("{id} initializes: {e}"))?;
        }
        // a store-backed module's state IS its store: it never installs (and
        // `WasmModule::install` refuses), so the source is not even asked.
        Start::Resume { snapshots } => {
            let installs_snapshots = shape.backing != Backing::Store;
            if installs_snapshots
                && let Some((snapshot, root)) = snapshots(id, shape.backing).await?
            {
                module
                    .install(&snapshot, root)
                    .map_err(|e| format!("{id} install: {e}"))?;
            }
        }
    }
    Ok(module)
}

/// Readiness is "a validator can run what the registry entry IS": for a
/// `Kind::Module` entry the consensus code (declared shape realizable here),
/// its optional mapper (matching its eventual index install) and its optional
/// view; for a `Kind::View` entry the view alone; for a `Kind::Plane` entry
/// nothing at all, because the artifact is not this boundary's; the owning
/// plane supplies the live restore proof separately. For the two
/// the boundary does realize, the frame's tag must be the entry's kind — a
/// view frame under a module id (or a module frame under a view id) is a
/// named refusal, never a vote. View validation
/// checks strict metadata and the canonical Ice ABI without instantiating or
/// executing the view; unknown imports follow the desktop host's trap policy,
/// so static acceptance does not guarantee that instantiation, init, or boot
/// will succeed.
pub fn validate_deployment(
    id: &str,
    kind: modules::Kind,
    bytes: &[u8],
    index: &indexer::IndexStore,
) -> Result<(), String> {
    workspace_config::validate_module_id(id)?;
    match kind {
        // a plane's artifact is not a deployment frame, and this boundary
        // never decodes it: the node plane that owns it realizes it, and only
        // that plane knows whether its live state can be restored. Static
        // deployment validation therefore has no answer for a plane.
        modules::Kind::Plane => Ok(()),
        modules::Kind::Module => match module_artifact::ArtifactRef::decode(bytes)? {
            module_artifact::ArtifactRef::Module(module) => validate_module(id, module, index),
            module_artifact::ArtifactRef::View(_) => Err(format!(
                "artifact_kind_mismatch: {id} is registered as a module, but the artifact is a view-only frame"
            )),
        },
        modules::Kind::View => match module_artifact::ArtifactRef::decode(bytes)? {
            module_artifact::ArtifactRef::View(view) => validate_view(view.component),
            module_artifact::ArtifactRef::Module(_) => Err(format!(
                "artifact_kind_mismatch: {id} is registered as a view, but the artifact is a module frame"
            )),
        },
    }
}

fn validate_module(
    id: &str,
    artifact: module_artifact::ModuleArtifactRef<'_>,
    index: &indexer::IndexStore,
) -> Result<(), String> {
    let shape =
        WasmModule::declared_shape(artifact.component).map_err(|error| error.to_string())?;
    check_realizable(id, &shape)?;
    if let Some(mapper) = artifact.index {
        index
            .validate_guest(mapper)
            .map_err(|error| error.to_string())?;
    }
    if let Some(view) = artifact.view {
        validate_view(view.component)?;
    }
    Ok(())
}

fn validate_view(bytes: &[u8]) -> Result<(), String> {
    view_wire::manifest::read_manifest(bytes)
        .ok_or_else(|| "invalid Ice view manifest".to_string())?;
    let engine = wasmtime::Engine::default();
    let component = wasmtime::component::Component::from_binary(&engine, bytes)
        .map_err(|error| format!("invalid Ice view component: {error:#}"))?;
    let mut linker = wasmtime::component::Linker::<()>::new(&engine);
    linker
        .define_unknown_imports_as_traps(&component)
        .map_err(|error| format!("invalid Ice view imports: {error:#}"))?;
    let pre = linker
        .instantiate_pre(&component)
        .map_err(|error| format!("invalid Ice view imports: {error:#}"))?;
    view_abi::ViewPre::new(pre).map_err(|error| format!("invalid Ice view ABI: {error:#}"))?;
    Ok(())
}

/// Can this host run a component of `shape` under `id`? Every config key must be one the
/// network binds. the same check a validator applies before it signals a
/// swap ready, so an admission the boundary could not realize is refused
/// before it is ever scheduled, never at the boundary of every validator.
pub fn check_realizable(id: &str, shape: &Shape) -> Result<(), String> {
    workspace_config::validate_module_id(id)?;
    for key in &shape.config {
        require_config_key(id, key)?;
    }
    Ok(())
}

/// Open the engine named by the component over this tenant's private state.
fn open_odb(
    id: &str,
    engine: Backing,
    substrates: &Substrates,
) -> Result<Box<dyn wasm_host::OdbBacking>, String> {
    let path = substrates.path(id)?;
    match engine {
        Backing::Odb => files_odb::FilesOdbBacking::open(id, path)
            .map(|backing| Box::new(backing) as Box<dyn wasm_host::OdbBacking>)
            .map_err(|error| format!("object storage open: {error}")),
        Backing::Git => forge_odb::ForgeOdbBacking::open(id, path, substrates.blobs.clone())
            .map(|backing| Box::new(backing) as Box<dyn wasm_host::OdbBacking>)
            .map_err(|error| format!("git storage open: {error}")),
        Backing::Map | Backing::Store => Err("component does not declare object storage".into()),
    }
}

/// commit a STORE-BACKED module's `__config` record from its declared config
/// keys; idempotent (a store already carrying one is left untouched).
async fn seed_store_config(
    store: &mut dyn MerkleStore,
    id: &str,
    shape: &Shape,
    bindings: &Bindings<'_>,
) -> Result<(), String> {
    if shape.config.is_empty() {
        return Ok(());
    }
    let key = sdk::store_key(sdk::genesis_config::CONFIG_KEY);
    let already = store
        .get(&key)
        .await
        .map_err(|e| format!("{id} genesis config read: {e}"))?;
    if already.is_some() {
        return Ok(());
    }
    let config = encode_config(id, shape, bindings)?;
    store
        .commit_batch(vec![(key, Some(config))])
        .await
        .map_err(|e| format!("{id} genesis config seeds: {e}"))
}

/// an ODB-BACKED module's `__config` bytes: `None` when the shape declares no
/// config keys, else the same encoding a store-backed twin would seed —
/// [`wasm_host::CompiledModule::over_odb`] carries it, not an install, since
/// an odb backing has no key/value plane of its own to seed into.
fn odb_genesis_config(
    id: &str,
    shape: &Shape,
    bindings: &Bindings<'_>,
) -> Result<Option<Vec<u8>>, String> {
    if shape.config.is_empty() {
        return Ok(None);
    }
    Ok(Some(encode_config(id, shape, bindings)?))
}

/// this module's `__config` bytes: every declared config key resolved against
/// the network bindings. the codec wants strictly increasing keys, so the
/// declaration's order does not matter and a duplicate collapses.
fn encode_config(id: &str, shape: &Shape, bindings: &Bindings<'_>) -> Result<Vec<u8>, String> {
    let keys: BTreeSet<&str> = shape.config.iter().map(String::as_str).collect();
    let mut params: Vec<(&str, &[u8])> = Vec::with_capacity(keys.len());
    for key in keys {
        params.push((key, config_value(id, key, bindings)?));
    }
    Ok(sdk::genesis_config::encode_config(&params))
}

/// the binding a declared config key resolves to; an unknown key is refused
/// by name (a component asking for a parameter no network binds).
fn config_value<'a>(id: &str, key: &str, bindings: &Bindings<'a>) -> Result<&'a [u8], String> {
    match key {
        sdk::genesis_config::INVITE => Ok(bindings.invite),
        sdk::genesis_config::CHAIN_ID => Ok(bindings.chain_id.as_bytes()),
        sdk::genesis_config::TIME_UNIT => Ok(bindings.time_unit.encode()),
        other => Err(unbound_config_key(id, other)),
    }
}

/// the keys [`config_value`] resolves, checked without a binding in hand.
fn require_config_key(id: &str, key: &str) -> Result<(), String> {
    let known = key == sdk::genesis_config::INVITE
        || key == sdk::genesis_config::CHAIN_ID
        || key == sdk::genesis_config::TIME_UNIT;
    if known {
        return Ok(());
    }
    Err(unbound_config_key(id, key))
}

fn unbound_config_key(id: &str, key: &str) -> String {
    format!(
        "module {id} declares config key {key:?}, which no network binds (known: {:?}, {:?}, {:?})",
        sdk::genesis_config::CHAIN_ID,
        sdk::genesis_config::INVITE,
        sdk::genesis_config::TIME_UNIT
    )
}

/// the [`host::ModuleFactory`] a composed host carries: a post-genesis
/// admission (governance `RegisterModule` → modules `ScheduleRegister`)
/// builds its module through [`wasm_module`] at the activation boundary —
/// starting fresh over a store the canonical source opens under its id, the
/// node's substrates, and the network bindings. the constructor twin of the
/// code source, and the same path a genesis tenant took at block zero.
pub struct Admissions {
    context: commonware_runtime::tokio::Context,
    substrates: Substrates,
    invite: Vec<u8>,
    chain_id: String,
    time_unit: sdk::genesis_config::TimeUnit,
}

impl Admissions {
    /// over the node's CANONICAL substrates and store root (a sync attempt's
    /// scratch dirs are never a home for a module admitted later). the
    /// runtime hands out owned contexts only as labeled children; a store's
    /// partitions are named by module id alone, so a child opens the same
    /// store the boot context would.
    pub fn new(
        context: &commonware_runtime::tokio::Context,
        substrates: &Substrates,
        bindings: &Bindings<'_>,
    ) -> Self {
        use commonware_runtime::Supervisor as _;
        Self {
            context: context.child("admissions"),
            substrates: substrates.clone(),
            invite: bindings.invite.to_vec(),
            chain_id: bindings.chain_id.to_string(),
            time_unit: bindings.time_unit,
        }
    }

    fn bindings(&self) -> Bindings<'_> {
        Bindings {
            invite: &self.invite,
            time_unit: self.time_unit,
            chain_id: &self.chain_id,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl host::ModuleFactory for Admissions {
    async fn instantiate(&self, id: &str, bytes: &[u8]) -> Result<host::Admitted, sdk::Error> {
        let mut stores = crate::bundle::qmdb_stores(&self.context);
        admit(id, bytes, &mut stores, &self.substrates, &self.bindings()).await
    }

    fn check(&self, id: &str, bytes: &[u8]) -> Result<(), sdk::Error> {
        check_admission(id, bytes, &self.bindings())
    }
}

/// one admission of `bytes` under `id`, over the stores and substrates it is
/// handed: the module seated fresh and initialized, or the answer that the
/// bytes are another plane's record.
async fn admit(
    id: &str,
    bytes: &[u8],
    stores: &mut StoreSource<'_>,
    substrates: &Substrates,
    bindings: &Bindings<'_>,
) -> Result<host::Admitted, sdk::Error> {
    // bytes carrying no artifact frame at all are no module: another
    // plane's record committed through the same id-generic registry. Skip
    // and latch — a hard error here is a permanent code stall on every
    // node, for bytes this boundary never owned.
    let Ok(artifact) = module_artifact::ArtifactRef::decode(bytes) else {
        return Ok(host::Admitted::ForeignAbi);
    };
    // the host never asks this factory for a `Kind::View` entry
    // (`Host::realize_module_swaps` skips them), so a view frame here is
    // a module entry whose bytes are no module: fail closed rather than
    // seat an empty core.
    let artifact = match artifact {
        module_artifact::ArtifactRef::Module(module) => module,
        module_artifact::ArtifactRef::View(_) => {
            return Err(sdk::Error::module(
                "artifact_kind_mismatch",
                format!("{id} is a module entry, but the artifact is a view-only frame"),
            ));
        }
    };
    let seated = wasm_module(
        id,
        bytes,
        stores,
        substrates,
        bindings,
        Start::Fresh {
            parameters: &sdk::genesis_config::encode_config(&[]),
        },
    )
    .await;
    let refusal = match seated {
        Ok(module) => return Ok(host::Admitted::Module(Box::new(module))),
        Err(refusal) => refusal,
    };
    // ONLY now: do these bytes even speak the module ABI? A `ducktape:
    // module` this build refused stays fail-closed (an older binary must
    // never silently seat a different registry set than its peers); bytes
    // that are no module at all are another plane's record, and this
    // boundary is not the plane that realizes them. The extra compile is
    // paid on the refusal path alone, and the host latches the answer.
    let is_a_module = wasm_host::speaks_module_abi(artifact.component);
    match is_a_module {
        true => Err(sdk::Error::module("module_seat", refusal)),
        false => Ok(host::Admitted::ForeignAbi),
    }
}

/// the admission the activation boundary will run for `bytes` under `id`,
/// run now over SCRATCH state and dropped: an in-memory store, a throwaway
/// substrate directory and blob store, the network's bindings. `initialize`
/// runs only when an admission is seated, identically on every node, so a
/// guest that refuses there would stop every node at its activation height;
/// asked here first, it refuses by name before anyone signals it ready.
/// Nothing the node runs or stores is touched.
pub fn check_admission(id: &str, bytes: &[u8], bindings: &Bindings<'_>) -> Result<(), sdk::Error> {
    let scratch = tempfile::tempdir().map_err(|error| {
        sdk::Error::module(
            "admission_scratch",
            format!("{id}: scratch directory: {error}"),
        )
    })?;
    let substrates = Substrates {
        directory: scratch.path().to_path_buf(),
        bindings: BTreeMap::new(),
        blobs: blobstore::BlobHandle::default(),
    };
    let mut stores = |_: &str| -> BoxFut<'static, Result<Box<dyn MerkleStore>, String>> {
        Box::pin(async { Ok(Box::new(ScratchStore::default()) as Box<dyn MerkleStore>) })
    };
    // nothing in a scratch admission waits on I/O — memory and a local
    // directory — so blocking on it costs the computation alone, like the
    // compile the readiness probe already pays.
    futures::executor::block_on(admit(id, bytes, &mut stores, &substrates, bindings)).map(drop)
}

/// the store a scratch admission starts over. Nothing reads its root: the
/// module it backs is dropped once it has started.
#[derive(Default)]
struct ScratchStore(BTreeMap<[u8; sdk::ROOT_LEN], Vec<u8>>);

#[async_trait::async_trait(?Send)]
impl MerkleStore for ScratchStore {
    async fn get(&self, key: &[u8; sdk::ROOT_LEN]) -> Result<Option<Vec<u8>>, sdk::Error> {
        Ok(self.0.get(key).cloned())
    }

    async fn commit_batch(
        &mut self,
        writes: Vec<([u8; sdk::ROOT_LEN], Option<Vec<u8>>)>,
    ) -> Result<(), sdk::Error> {
        for (key, value) in writes {
            match value {
                Some(value) => self.0.insert(key, value),
                None => self.0.remove(&key),
            };
        }
        Ok(())
    }

    fn root(&self) -> StateRoot {
        StateRoot([0; 32])
    }

    async fn sync_target(&self) -> Result<sdk::ResolverSyncTarget, sdk::Error> {
        Err(no_sync_lane())
    }

    async fn serve_sync(&self, _req: &[u8]) -> Result<Vec<u8>, sdk::Error> {
        Err(no_sync_lane())
    }
}

fn no_sync_lane() -> sdk::Error {
    sdk::Error::module("scratch_store", "a scratch store has no sync lane")
}
