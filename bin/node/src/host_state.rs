//! Application-host construction, restoration, and state synchronization.
//!
//! This module owns the three host lifecycles — genesis, checkpoint restore,
//! and state sync — over the ONE composer ([`noded::compose`]). Each lifecycle
//! injects only what differs: WHERE its stores come from (a fresh/reopened
//! `QmdbStore::init`, or a `sync_from` at a verified root), WHERE its
//! snapshots come from (the checkpoint, or the peer's snapshot lane), and
//! WHICH boundary it composes at (block zero over the genesis bundle, or the
//! modules registry's roster at the checkpoint or manifest height). Every
//! wasm module's shape is its component's own declaration; the genesis wasm
//! is the workspace genesis file's — every component verified against the
//! descriptor's hashes and seeded into the node's blob plane, every index
//! guest converged into the index, never embedded in the binary; and the
//! network bindings ride every path, so a module the registry admitted after
//! a checkpoint seeds its config at restore exactly as it did live. Every
//! host leaves here wired: the admissions factory over the node's canonical
//! substrates, and an index database for every module it runs.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};

use commonware_cryptography::ed25519;
use commonware_runtime::Supervisor as _;
use duckfs_disk::SyncScratch;
use files::Files;
use host::Host;
use noded::bundle::qmdb_stores;
use noded::compose::{
    Admissions, Bindings, Boot, BoxFut, Start, Substrates, compose, fetch_code, wasm_module,
};
use recovery::Manifest;
use sdk::StateRoot;
use sha2::Digest as _;
use statesync::{
    fetch_snapshot,
    qmdb::{QmdbStore, RemoteQmdbSource},
};
#[cfg(test)]
use topology::PRODUCTION;
use wasm_host::Backing;

use noded::{converge_host_modules, index_host_modules};

use crate::config::{
    Genesis, GenesisModules, GenesisSource, component_path, hex_bytes, install_genesis,
    modules_path, verify_genesis,
};
use crate::util::hex;

/// what a reopen installs into a map or odb tenant: `(snapshot bytes, root)`,
/// or `None` when the tenant's state is already on its own disk substrate.
type Snapshot = Option<(Vec<u8>, StateRoot)>;

/// Consensus-visible network names shared by genesis, restore, and state sync.
#[derive(Clone, Copy)]
pub(super) struct NetworkBindings<'a> {
    pub(super) invite: &'a [u8],
    pub(super) identity_chain_id: &'a str,
}

impl<'a> NetworkBindings<'a> {
    fn compose(&self) -> Bindings<'a> {
        Bindings {
            invite: self.invite,
            chain_id: self.identity_chain_id,
            // NOT a per-call binding, because it does not vary here: no lane
            // this binary composes — validator, replica, sync-only — installs
            // a `ConsensusTimePolicy`, so `consensus_time` IS the height on
            // every one of them, which is what each reports as its
            // `consensus_time_unit`. The sim lane, which arms
            // `ConsensusTimePolicy::Epoch`, binds `Millis` in `bin/simnode`.
            time_unit: sdk::genesis_config::TimeUnit::Height,
        }
    }
}

/// the node-local substrates every host construction composes over — genesis,
/// restore, and state sync alike: forge's git repo, files' duckfs dir, the
/// blob store the genesis components are hydrated into, and the derived index
/// the genesis's guests converge into.
pub(super) struct NodeSubstrates<'a> {
    pub(super) forge_repo: &'a std::path::Path,
    pub(super) duckfs_dir: &'a std::path::Path,
    pub(super) blobs: blobstore::BlobHandle,
    pub(super) index: &'a indexer::IndexStore,
}

/// the blobstore-backed [`host::CodeSource`]: component bytes — the genesis
/// file's, seeded by [`hydrate_genesis`], and a code swap's, staged there before
/// the governance schedule exactly like a forge Push packfile — are
/// content-addressed chunks on the node's blob plane. a hash the store lacks
/// is a `None` — the boundary fails closed rather than forking.
pub(super) struct BlobCodeSource(pub(super) std::sync::Arc<dyn blobstore::Blobs>);

#[async_trait::async_trait(?Send)]
impl host::CodeSource for BlobCodeSource {
    async fn fetch(&self, code_hash: &[u8]) -> Option<Vec<u8>> {
        let digest: [u8; 32] = code_hash.try_into().ok()?;
        self.0.get_chunk(&digest)
    }

    fn origin(&self) -> &'static str {
        "blob_local"
    }
}

/// what [`hydrate_from_disk`] found on disk.
enum Hydrated {
    /// Every genesis deployment is in the blob store; composition installs
    /// mappers from the actual running deployments.
    Installed,
    /// the workspace genesis file is absent — a joiner before its first
    /// fetch — and nothing was installed. `hash` is the descriptor's pin the
    /// fetch asks the mesh for.
    GenesisAbsent {
        file: std::path::PathBuf,
        hash: [u8; 32],
    },
}

/// install the genesis this node holds on disk: every component into the
/// (persistent) blob store, verified by name against the descriptor's hash;
/// the workspace genesis file itself as a chunk keyed by its own hash, so this
/// node serves it to the next joiner; and every index guest into its module's
/// index database. Idempotent: a chunk the store already holds is not re-put,
/// and an index database already holding its guest converges for free.
fn hydrate_from_disk(
    blobs: &blobstore::BlobHandle,
    index: &indexer::IndexStore,
    genesis: &GenesisModules,
) -> Result<Hydrated, String> {
    match &genesis.source {
        GenesisSource::FoundingSet(dir) => {
            seed_founding_set(blobs, dir, &genesis.hashes)?;
        }
        GenesisSource::Workspace { file, hash } => {
            let Some(loaded) = seed_workspace_genesis(blobs, file, hash, &genesis.hashes)? else {
                return Ok(Hydrated::GenesisAbsent {
                    file: file.clone(),
                    hash: *hash,
                });
            };
            // the verified genesis, unpacked beside network.toml as bare files
            let workspace = file
                .parent()
                .ok_or_else(|| format!("{} has no workspace directory", file.display()))?;
            loaded.materialize(&modules_path(workspace))?;
        }
    };
    index_host_modules(index, genesis.hashes.keys().map(String::as_str))?;
    Ok(Hydrated::Installed)
}

/// [`hydrate_from_disk`] for a node with no mesh to fetch from — a founder, a
/// member, or a workspace reopening its checkpoint: an absent genesis is a
/// refusal naming the file and how it gets there.
pub(super) fn hydrate_genesis(
    blobs: &blobstore::BlobHandle,
    index: &indexer::IndexStore,
    genesis: &GenesisModules,
) -> Result<(), String> {
    match hydrate_from_disk(blobs, index, genesis)? {
        Hydrated::Installed => Ok(()),
        Hydrated::GenesisAbsent { file, .. } => Err(format!(
            "{} is missing — fail-closed. a genesis member boots from its own genesis: \
             re-run `ducktape node join <invite> --genesis <file>` with the founder's \
             genesis file, or re-found with `ducktape node init`",
            file.display()
        )),
    }
}

/// the joiner's twin of [`hydrate_genesis`]: a workspace that lacks its
/// genesis fetches it off the mesh by the descriptor's pin (the ranged blob
/// lane — every node seeded its own genesis file into its blob store as a
/// chunk keyed by that hash), installs it beside `network.toml`, then
/// hydrates exactly as a member does. Runs inside the replica's forever-retry
/// loop, so the fetch is announced on attempt 1, never per try.
pub(super) async fn fetch_and_hydrate_genesis<
    C: statesync::SyncClient + crate::blob_fetch::SourceRotate,
>(
    client: &C,
    blobs: &blobstore::BlobHandle,
    index: &indexer::IndexStore,
    genesis: &GenesisModules,
    attempt: usize,
) -> Result<(), String> {
    let Hydrated::GenesisAbsent { hash, .. } = hydrate_from_disk(blobs, index, genesis)? else {
        return Ok(());
    };
    let first_attempt = attempt <= 1;
    if first_attempt {
        tracing::info!(
            target: "ducktape::boot",
            genesis = %hex_bytes(&hash),
            "genesis absent from the workspace; fetching it from the mesh"
        );
    }
    crate::blob_fetch::fetch_blob(
        client,
        blobs,
        &hash,
        crate::constants::MAX_MODULE_CODE_BYTES,
        crate::constants::BLOB_FETCH_ATTEMPTS,
    )
    .await
    .map_err(|e| format!("fetch genesis {}: {e}", hex_bytes(&hash)))?;
    // the fetch landed the genesis in the blob store under its pin, which is
    // exactly where a missing workspace file is rewritten from.
    match hydrate_from_disk(blobs, index, genesis)? {
        Hydrated::Installed => Ok(()),
        Hydrated::GenesisAbsent { hash, .. } => Err(format!(
            "fetched genesis {} is not in the blob store",
            hex_bytes(&hash)
        )),
    }
}

/// seed the blob store from a workspace genesis: `None` when this node holds
/// no genesis at all; otherwise the decoded genesis, every component the
/// store lacked put (each verified against `want` first — a tampered file
/// refuses by module id, and the whole file by its hash) and the file itself
/// put as a chunk keyed by `hash`. The blob store holds every genesis this
/// node ever installed or fetched under its pin, so the file is that chunk's
/// readable copy: a missing file with the chunk present is rewritten from it.
fn seed_workspace_genesis(
    blobs: &blobstore::BlobHandle,
    file: &std::path::Path,
    hash: &[u8; 32],
    want: &std::collections::BTreeMap<String, [u8; 32]>,
) -> Result<Option<Genesis>, String> {
    let bytes = match read_optional(file)? {
        Some(bytes) => bytes,
        None => {
            let Some(bytes) = blobs.get_chunk(hash) else {
                return Ok(None);
            };
            let workspace = file
                .parent()
                .ok_or_else(|| format!("{} has no workspace directory", file.display()))?;
            install_genesis(workspace, hash, want, &bytes)?;
            tracing::info!(
                target: "ducktape::boot",
                genesis = %hex_bytes(hash),
                file = %file.display(),
                "genesis file rewritten from the blob store"
            );
            bytes
        }
    };
    let genesis =
        verify_genesis(&bytes, hash, want).map_err(|e| format!("{}: {e}", file.display()))?;
    let mut seeded = 0usize;
    for (id, digest) in want {
        // the VERIFYING query: seeding is what heals a corrupt component, and a
        // stat-shaped "present" would skip exactly the file that needs it.
        if blobs.has_verified_chunk(digest) {
            continue;
        }
        // verified above: every id in `want` is a component hashing to `digest`.
        let component = genesis
            .artifact(id)
            .expect("verified genesis carries every module");
        blobs.put_chunk(component.to_vec());
        seeded += 1;
    }
    if !blobs.has_verified_chunk(hash) {
        blobs.put_chunk(bytes);
    }
    // a lifecycle fact: bytes entered the store (a restart over a seeded store
    // seeds nothing and says nothing).
    let seeded_any = seeded > 0;
    if seeded_any {
        tracing::info!(
            target: "ducktape::boot",
            modules = want.len(),
            seeded,
            genesis = %file.display(),
            "genesis seeded into the blob store"
        );
    }
    Ok(Some(genesis))
}

/// seed the blob store from a founding set — the dev shape, whose files ARE
/// its genesis code set: every component the store lacks is read from
/// `<dir>/<id>.component.wasm`, verified against `want`, and put. A missing
/// file refuses by module id and path (the set was hashed at resolve, so an
/// absence now is a set that changed under the node).
fn seed_founding_set(
    blobs: &blobstore::BlobHandle,
    dir: &std::path::Path,
    want: &std::collections::BTreeMap<String, [u8; 32]>,
) -> Result<(), String> {
    let mut seeded = 0usize;
    for (id, digest) in want {
        // verifying, for the same reason as the genesis seed above.
        if blobs.has_verified_chunk(digest) {
            continue;
        }
        let path = component_path(dir, id);
        let bytes = workspace_config::read_module_artifact(dir, id)?.encode();
        let got: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        let matches_descriptor = got == *digest;
        if !matches_descriptor {
            return Err(format!(
                "module {id}: {} hashes to {} but the descriptor says {} — fail-closed",
                path.display(),
                hex_bytes(&got),
                hex_bytes(digest)
            ));
        }
        blobs.put_chunk(bytes);
        seeded += 1;
    }
    let seeded_any = seeded > 0;
    if seeded_any {
        tracing::info!(
            target: "ducktape::boot",
            modules = want.len(),
            seeded,
            founding_set = %dir.display(),
            "founding set seeded into the blob store"
        );
    }
    Ok(())
}

/// a file's bytes; `None` when there is no file at `path`. any other failure
/// names the path (the operator's next move is to look at that directory).
fn read_optional(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) => match e.kind() {
            std::io::ErrorKind::NotFound => Ok(None),
            other => Err(format!("read {} ({other:?}): {e}", path.display())),
        },
    }
}

/// the host-side disk substrates the odb-backed tenants open over.
fn disk_substrates(
    forge_repo: &std::path::Path,
    duckfs_dir: &std::path::Path,
    blobs: blobstore::BlobHandle,
) -> Substrates {
    Substrates {
        forge_repo: forge_repo.to_path_buf(),
        duckfs_dir: duckfs_dir.to_path_buf(),
        blobs,
    }
}

/// what every composed host carries before it runs a block: the admissions
/// factory over the node's CANONICAL substrates and bindings (a module the
/// registry admits later builds through the same path a genesis tenant took),
/// and an index database for every module it runs (one admitted after the
/// index opened gets its database here, before its first block folds).
fn wire(
    host: &mut Host,
    context: &commonware_runtime::tokio::Context,
    substrates: &Substrates,
    net: &NetworkBindings<'_>,
    index: &indexer::IndexStore,
) -> Result<(), String> {
    host.set_module_factory(Box::new(Admissions::new(
        context,
        substrates,
        &net.compose(),
    )));
    converge_host_modules(index, host)
}

/// The workspace's module set at block zero, authenticated by its genesis
/// deployment hashes. Every component initializes through the same Wasm
/// lifecycle, with valset consuming the genesis
/// validators and the modules registry with the descriptor's code hashes.
pub(super) async fn genesis_host(
    context: &commonware_runtime::tokio::Context,
    genesis_validators: &[ed25519::PublicKey],
    net: NetworkBindings<'_>,
    substrates: NodeSubstrates<'_>,
    genesis: &GenesisModules,
) -> Result<Host, String> {
    let NodeSubstrates {
        forge_repo,
        duckfs_dir,
        blobs,
        index,
    } = substrates;
    hydrate_genesis(&blobs, index, genesis)?;
    let validators: Vec<Vec<u8>> = genesis_validators
        .iter()
        .map(|k| k.as_ref().to_vec())
        .collect();
    let code = BlobCodeSource(std::sync::Arc::new(blobs.clone()));
    let mut stores = qmdb_stores(context);
    let substrates = disk_substrates(forge_repo, duckfs_dir, blobs);
    let mut host = compose(
        &code,
        &mut stores,
        &substrates,
        &net.compose(),
        Boot::Genesis {
            validators: &validators,
            bundle: &genesis.hashes,
        },
    )
    .await
    .map_err(|e| format!("genesis compose: {e}"))?;
    wire(&mut host, context, &substrates, &net, index)?;
    Ok(host)
}

/// Validate the executable roster before opening any module state.
fn module_codes<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<BTreeMap<String, [u8; 32]>, String> {
    let mut codes = BTreeMap::new();
    for (id, hash) in entries {
        workspace_config::validate_module_id(id)?;
        let hash = hash
            .try_into()
            .map_err(|_| format!("module {id} has no 32-byte executable commitment"))?;
        if codes.insert(id.to_owned(), hash).is_some() {
            return Err(format!("duplicate executable commitment for {id}"));
        }
    }
    Ok(codes)
}

/// the RESTORE twin of [`genesis_host`]: the disk substrates (qmdb stores,
/// forge's git repo, files' duckfs dir) reopen themselves at their own
/// committed positions; the map cohort installs its checkpoint snapshots,
/// root-checked. the wasm set is the modules registry's roster on the code
/// it designates for the checkpoint height — the registry is per-block
/// durable and reopens AHEAD of the checkpoint, so a swap it committed past
/// the checkpoint is seated by the replay's own boundary realization, and a
/// module it admitted past the checkpoint starts fresh here (no state to
/// resume) and is rebuilt by the replay exactly as the live node built it.
/// the recovery replay then rolls everything forward to the journal tip.
pub(super) async fn restore_host(
    context: &commonware_runtime::tokio::Context,
    manifest: &Manifest,
    net: NetworkBindings<'_>,
    substrates: NodeSubstrates<'_>,
    genesis: &GenesisModules,
) -> Result<Host, String> {
    let NodeSubstrates {
        forge_repo,
        duckfs_dir,
        blobs,
        index,
    } = substrates;
    hydrate_genesis(&blobs, index, genesis)?;
    let code = BlobCodeSource(std::sync::Arc::new(blobs.clone()));
    let mut stores = qmdb_stores(context);
    let mut snapshots = |id: &str, backing: Backing| -> BoxFut<'_, Result<Snapshot, String>> {
        let got = restore_snapshot(manifest, id, backing);
        Box::pin(async move { got })
    };
    // a genesis checkpoint has applied nothing.
    let height = manifest.height.unwrap_or(0);
    let substrates = disk_substrates(forge_repo, duckfs_dir, blobs);
    let mut host = compose(
        &code,
        &mut stores,
        &substrates,
        &net.compose(),
        Boot::Reopen {
            height,
            codes: &module_codes(
                manifest
                    .codes
                    .iter()
                    .map(|(id, hash)| (id.as_str(), hash.as_slice())),
            )?,
            snapshots: &mut snapshots,
        },
    )
    .await
    .map_err(|e| format!("restore compose: {e}"))?;
    wire(&mut host, context, &substrates, &net, index)?;
    // the committed clock an authenticated read is answered against. This path
    // applies no block, so without it a node reopened at height 40 000 answers
    // reads with time 0 until its next commit — and an expiry check against 0
    // reads as "nothing has expired", fail-open, in the restart window.
    // `consensus_time` IS the height on this lane (recovery's own `BlockContext`
    // builds it that way), which is why one number serves both.
    host.restore_committed(height, height);
    Ok(host)
}

/// what a checkpoint restore installs for a tenant the checkpoint captured: a
/// map tenant's snapshot (the checkpoint holds it, and one it lacks is a
/// checkpoint that cannot restore this module — an error, never an empty
/// module); an odb tenant (files, forge) reopens its own disk substrate at
/// its committed position and installs nothing. a store-backed tenant is
/// never asked: its state IS its store.
fn restore_snapshot(manifest: &Manifest, id: &str, backing: Backing) -> Result<Snapshot, String> {
    match backing {
        Backing::Map => manifest_snapshot(manifest, id).map(Some),
        Backing::Store | Backing::Odb => Ok(None),
    }
}

/// the checkpoint's `(snapshot, root)` for `id`; both must be present.
fn manifest_snapshot(manifest: &Manifest, id: &str) -> Result<(Vec<u8>, StateRoot), String> {
    let bytes = manifest
        .snapshot(id)
        .ok_or_else(|| format!("checkpoint has no snapshot for module {id}"))?;
    let root = manifest
        .root(id)
        .ok_or_else(|| format!("checkpoint has no root for module {id}"))?;
    Ok((bytes.to_vec(), root))
}

/// the object-store ([`statesync::ObjectFetch`]) adapter over the live `files`
/// module: the statesync possession driver owns the loop + the full-possession
/// gate, this owns the duckfs `serve_sync` wire (refs image + `GetObjects`).
///
/// SCRATCH NAMESPACE (#219): like the qmdb modules — whose `sync_from` lands
/// under an ATTEMPT-scoped runtime child (`{name}_scratch_a{n}`) — the module
/// this adapter wraps is opened over `duckfs_disk::SyncScratch`'s attempt-scoped
/// scratch dir, NEVER the canonical `duckfs_dir`. the canonical dir is written
/// only by the verified promotion after `sync_all_modules`' composite root-hash
/// gate, so a failed join leaves it byte-untouched.
struct FilesOdb<'a>(&'a mut Files);

impl statesync::ObjectFetch for FilesOdb<'_> {
    fn refs_request(&self) -> Vec<u8> {
        duckfs_core::encode_get_refs()
    }

    fn install_refs(&mut self, reply: &[u8], root: StateRoot, height: u64) -> Result<(), String> {
        let bytes = duckfs_core::decode_refs_reply(reply)?;
        // persist the refs envelope at the SYNCED boundary height so a restart
        // right after the join resumes replay from the boundary, not genesis.
        self.0
            .install(&bytes, root, height)
            .map_err(|e| e.to_string())
    }

    fn missing_request(&self, limit: usize) -> Result<Option<Vec<u8>>, String> {
        let ids = self.0.missing_objects(limit).map_err(|e| e.to_string())?;
        if ids.is_empty() {
            return Ok(None);
        }
        Ok(Some(duckfs_core::encode_get_objects(&ids)))
    }

    fn ingest(&mut self, reply: &[u8]) -> Result<usize, String> {
        let batch = duckfs_core::decode_objects_reply(reply)?;
        let landed = batch.len();
        self.0.ingest_objects(&batch).map_err(|e| e.to_string())?;
        Ok(landed)
    }

    fn possession_complete(&self) -> Result<bool, String> {
        self.0.possession_complete().map_err(|e| e.to_string())
    }
}

/// intern a runtime-child / metric label as a process-wide `&'static str`: a
/// repeat call with the same text returns the SAME leaked string instead of
/// leaking a new one. `sync_all_modules` re-enters forever on a resident
/// whose boundary sync keeps failing (`replica/park.rs`'s retry loop never
/// bails while `resident_standing`), so a per-call label here must not leak —
/// only a genuinely new label (a module id never seen before) costs a leak.
fn intern_label(name: &str) -> &'static str {
    static LABELS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let cache = LABELS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut cache = cache.lock().unwrap();
    if let Some(existing) = cache.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    cache.insert(leaked);
    leaked
}

/// why a boundary sync did not produce a host.
#[derive(Debug)]
pub(super) enum SyncModulesError {
    /// the SOURCE's own manifest says one of its modules cannot be
    /// transferred at all (`PayloadKind::Unsupported`: the serving node could
    /// not prepare a sync handle for it). that is a stable fact about the
    /// source, not about this attempt — no refetch of its manifest turns it
    /// into a transferable boundary — so the caller rotates instead of
    /// retrying.
    SourceDegraded(String),
    /// everything else: a transfer that may well complete on the next attempt
    /// (a moved qmdb target, a busy peer, a dropped chunk).
    Failed(String),
}

impl SyncModulesError {
    /// the stable snake_case token the failure logs and metrics carry.
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Self::SourceDegraded(_) => "source_degraded_module",
            Self::Failed(_) => "sync_failed",
        }
    }
}

impl std::fmt::Display for SyncModulesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceDegraded(module) => {
                write!(f, "source cannot transfer module {module} at this boundary")
            }
            Self::Failed(e) => f.write_str(e),
        }
    }
}

impl From<String> for SyncModulesError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

/// the source-degraded refusal for `manifest`, if it carries one — the whole
/// decision, so it is testable without a peer.
fn undeliverable_boundary(manifest: &statesync::Manifest) -> Option<SyncModulesError> {
    manifest
        .undeliverable_module()
        .map(|module| SyncModulesError::SourceDegraded(module.to_string()))
}

/// rebuild EVERY module of the modules registry's roster from a peer's
/// statesync service at `manifest`'s boundary and compose them into a
/// [`Host`], verified against the manifest's root-hash. the disk substrates
/// land under their canonical ids in this process's storage root — this IS
/// the node's state afterwards, not a scratch copy. `attempt` disambiguates
/// scratch DIRECTORIES on disk across retries (a busy source moves its qmdb
/// targets past the captured boundary; the caller refetches the manifest and
/// tries again) — the runtime child / metric LABELS stay attempt-independent
/// and interned (`intern_label`), since a resident retries this forever and a
/// label per attempt would leak and grow `/metrics` without bound. every wasm
/// tenant joins on
/// the code the registry designates for the boundary, fetched off the blob
/// plane (the mesh behind it) and hash-checked by the composer.
pub(super) async fn sync_all_modules<C: statesync::SyncClient + crate::blob_fetch::SourceRotate>(
    context: &commonware_runtime::tokio::Context,
    client: &C,
    manifest: &statesync::Manifest,
    net: NetworkBindings<'_>,
    substrates: NodeSubstrates<'_>,
    attempt: usize,
    genesis: &GenesisModules,
) -> Result<Host, SyncModulesError> {
    // BEFORE a byte moves: a module this source cannot transfer makes the
    // whole boundary unbuildable (the composite root-hash gate below is over
    // the WHOLE roster), and every lane below would discover it as its own
    // opaque, retryable-looking failure — a store tenant as "missing pinned
    // resolver target", a map tenant as a snapshot fetch the source cannot
    // answer. read it off the manifest and let the caller rotate.
    if let Some(degraded) = undeliverable_boundary(manifest) {
        return Err(degraded);
    }
    let NodeSubstrates {
        forge_repo,
        duckfs_dir,
        blobs,
        index,
    } = substrates;
    // a joiner's workspace holds no genesis until this fetches it (`node
    // join` writes none for a non-member): the whole file comes off the mesh
    // by the descriptor's pin and installs beside network.toml, so after this
    // every genesis component is in the blob store and every index guest is
    // in the index. `FetchingCodeSource` below still covers a component the
    // code registry swapped in or admitted after genesis; the composer
    // sha256-checks every fetched byte against the committed hash either way.
    fetch_and_hydrate_genesis(client, &blobs, index, genesis, attempt).await?;
    let entry_root = |module: &str| -> Result<StateRoot, String> {
        Ok(manifest
            .entry(module)
            .ok_or_else(|| format!("module {module} missing from the manifest"))?
            .root)
    };
    let scratch_context = context.child(intern_label("sync_scratch"));
    // the runtime labels its children with static strings, and the resolver
    // lane names its module the same way; a module id comes off the registry
    // as a `String`, so this interns rather than leaking a fresh string on
    // every call — `sync_all_modules` retries forever on a resident whose
    // boundary sync keeps failing, and a leak per attempt is a leak forever.
    let static_label = intern_label;
    let pinned_target = |module: &'static str| -> Result<statesync::qmdb::SyncTarget, String> {
        let entry = manifest
            .entry(module)
            .ok_or_else(|| format!("module {module} missing from the manifest"))?;
        let pinned = entry
            .resolver_target
            .as_ref()
            .ok_or_else(|| format!("module {module} missing pinned resolver target"))?;
        pinned.to_sync_target().map_err(|e| format!("{module} {e}"))
    };

    // resolver lane: adopt the manifest's pinned target, then fetch only
    // boundary-scoped op batches through the remote resolver.
    let fetch_target = |module: &'static str| {
        let resolver = RemoteQmdbSource::new(client.clone(), manifest.boundary_id(), module);
        async move {
            let target = pinned_target(module)?;
            let root = entry_root(module)?;
            if StateRoot(target.root.0) != root {
                return Err(format!(
                    "{module} pinned target root does not match the manifest root"
                ));
            }
            Ok::<_, String>((target, resolver))
        }
    };
    // snapshot lane: chunked bytes from the captured boundary, install gated
    // on the manifest root (verify-then-adopt inside each module).
    let snapshot_of = |module: &str| {
        let client = client.clone();
        let boundary = manifest.boundary_id();
        let root = entry_root(module);
        let module = module.to_string();
        async move {
            let root = root?;
            let bytes = fetch_snapshot(&client, boundary, &module, statesync::MAX_SNAPSHOT_BYTES)
                .await
                .map_err(|e| format!("{module} snapshot: {e}"))?;
            Ok::<_, String>((bytes, root))
        }
    };

    // files is a duckfs-odb resolver module: its refs image AND its
    // content-addressed objects both ride the Module/`serve_sync` lane. a fresh
    // joiner's odb is EMPTY, so install the boundary refs (root-verified) at the
    // sync-target height and then loop GetObjects to full object possession —
    // the snapshot lane would leave this node refs-only (every file listed, not
    // one byte readable). the sync lands in an ATTEMPT-scoped scratch dir
    // (`duckfs_scratch_a{attempt}`, mirroring the qmdb scratch namespaces);
    // the canonical `duckfs_dir` is written only by the verified promotion
    // after the composite root-hash gate below (#219).
    let files_scratch =
        SyncScratch::prepare(duckfs_dir, attempt).map_err(|e| format!("duckfs scratch: {e}"))?;
    // possession is node-local verification machinery, OFF consensus: drive it
    // with the NATIVE `Files` exactly as before (the joiner methods — install /
    // missing_objects / ingest / possession_complete — live on `Files`, not the
    // odb backing). it installs the boundary refs (at the sync-target height) and
    // ingests objects, all fsynced durably to the scratch dir.
    let mut files_possession = Files::open("files", files_scratch.dir().to_path_buf())
        .map_err(|e| format!("duckfs open: {e}"))?;
    let files_root = entry_root("files")?;
    let files_lane = statesync::ClientModuleLane::new(client.clone(), manifest.boundary_id());
    statesync::sync_object_possession(
        &files_lane,
        "files",
        files_root,
        manifest.height,
        &mut FilesOdb(&mut files_possession),
        duckfs_core::MAX_SYNC_IDS,
    )
    .await
    .map_err(|e| format!("files sync: {e}"))?;
    // possession wrote the synced refs envelope + every object durably to the
    // scratch dir, so drop that native handle and compose the ROOT-CONTINUOUS
    // wasm files tenant over the SAME scratch dir for the composite root-hash gate
    // below — `FilesOdbBacking::open` recovers exactly the possession-synced refs
    // (and its `durable_commit_height` from the envelope), so `root() =
    // sha256(refs_bytes)` certifies against the manifest's files root.
    drop(files_possession);

    // the composer's three sources on this path: code from the blob plane (the
    // mesh behind it — a joiner whose bundle trails a committed component
    // fetches it), every store rebuilt at the manifest's pinned target
    // (merkle-verified against the committed root) under the attempt-scoped
    // scratch namespace, and snapshots off the peer's snapshot lane.
    let code = crate::blob_fetch::FetchingCodeSource::new(
        blobs.clone(),
        client.clone(),
        crate::constants::MAX_MODULE_CODE_BYTES,
        crate::constants::BLOB_FETCH_ATTEMPTS,
    );
    let mut stores = |module: &str| -> BoxFut<'_, Result<Box<dyn sdk::MerkleStore>, String>> {
        let module = static_label(module);
        let child = scratch_context.child(static_label(&format!("{module}_scratch")));
        let target = fetch_target(module);
        Box::pin(async move {
            let (target, resolver) = target.await?;
            let store = QmdbStore::sync_from(child, module, target, resolver).await?;
            Ok(Box::new(store) as Box<dyn sdk::MerkleStore>)
        })
    };
    // files' refs image and objects are possession-synced above, into the
    // scratch dir the composer opens its substrate over; every other map or
    // odb tenant installs the peer's boundary snapshot (forge's refs image
    // rides the snapshot lane over the host-side git substrate). a
    // store-backed tenant is never asked: its state arrives through its store.
    let mut snapshots = |module: &str, _backing: Backing| -> BoxFut<'_, Result<Snapshot, String>> {
        let possession_synced_outside = module == "files";
        if possession_synced_outside {
            return Box::pin(async { Ok(None) });
        }
        let fetch = snapshot_of(module);
        Box::pin(async move { fetch.await.map(Some) })
    };
    let bindings = net.compose();
    let mut host = compose(
        &code,
        &mut stores,
        &disk_substrates(forge_repo, files_scratch.dir(), blobs.clone()),
        &bindings,
        Boot::Reopen {
            height: manifest.height,
            codes: &module_codes(manifest.entries.iter().map(|entry| {
                (
                    entry.module_id.as_str(),
                    entry.code_hash.as_deref().unwrap_or_default(),
                )
            }))?,
            snapshots: &mut snapshots,
        },
    )
    .await
    .map_err(|e| format!("sync compose: {e}"))?;
    // compose and check THE property: the rebuilt root-hash IS the manifest's.
    // the registry's roster keeps this set in lockstep with what the source
    // runs by construction — a missing module composes a different root-hash
    // and the join fails its final check.
    if host.root_hash() != manifest.root_hash {
        return Err(SyncModulesError::Failed(format!(
            "composed {} != manifest {}",
            hex(&host.root_hash()),
            hex(&manifest.root_hash)
        )));
    }
    // the composite gate passed — promote files' scratch into the canonical
    // `duckfs_dir` (verify-then-replace refs + content-addressed object merge,
    // gated on the exact files root this composition certified) and swap the
    // registry onto a canonical-backed module, built through the one wasm
    // path on the very code the composition seated. the returned host must
    // run in place over the canonical dir: the post-reboot full-sync fallback
    // keeps it live without a reboot, and a joiner's promotion reboot re-opens
    // the same dir. on any error the host is discarded and the retry re-syncs
    // — an already-promoted canonical dir is verified state, never damage.
    files_scratch
        .promote(files_root.0)
        .map_err(|e| format!("duckfs promote: {e}"))?;
    let files_code: [u8; 32] = host
        .module_code_hash("files")
        .ok_or_else(|| "files composed with no code hash".to_string())?
        .try_into()
        .map_err(|_| "files' code hash is not 32 bytes".to_string())?;
    let files_bytes = fetch_code(&code, "files", &files_code).await?;
    let canonical_substrates = disk_substrates(forge_repo, duckfs_dir, blobs.clone());
    let canonical_files = wasm_module(
        "files",
        &files_bytes,
        &mut stores,
        &canonical_substrates,
        &bindings,
        Start::Resume {
            snapshots: &mut snapshots,
        },
    )
    .await
    .map_err(|e| format!("duckfs reopen: {e}"))?;
    host.register(Box::new(canonical_files));
    // re-check THE property against the canonical-backed composition.
    if host.root_hash() != manifest.root_hash {
        return Err(SyncModulesError::Failed(format!(
            "canonical duckfs reopen composed {} != manifest {}",
            hex(&host.root_hash()),
            hex(&manifest.root_hash)
        )));
    }
    wire(&mut host, context, &canonical_substrates, &net, index)?;
    // same reason as the checkpoint reopen: a joiner that synced to this
    // boundary applied no block, so its committed clock has to be set from the
    // boundary it just proved rather than left at genesis.
    host.restore_committed(manifest.height, manifest.height);
    Ok(host)
}

#[cfg(test)]
mod tests {
    use commonware_runtime::Runner as _;

    use super::*;

    /// The PRODUCTION genesis root hash over [`PIN_BINDINGS`] and an EMPTY
    /// validator set — the consensus root every node of such a network computes
    /// at block zero, pinned so that moving it is a decision instead of an
    /// accident. Update it ONLY as the deliberate half of a flag day (see
    /// [`production_genesis_root_hash_is_pinned`]).
    const GENESIS_ROOT_HASH: &str =
        "2ceb01f889c0757eb062b3bdbaab38a0245f4b2289bdd729967e701fbe1c0387";

    /// The bindings [`GENESIS_ROOT_HASH`] is taken over. They are constants
    /// because they are NOT: each rides its module's genesis `__config`
    /// record (the composer's `seed_store_config` for a store-backed tenant,
    /// the `initial_state` install for a Map-backed one), so a real
    /// network's invite namespace and chain id put it on its own root by
    /// design. Pinning a hash only says anything against fixed ones.
    const PIN_BINDINGS: NetworkBindings<'static> = NetworkBindings {
        invite: b"parity-test",
        identity_chain_id: "parity-test",
    };

    /// Compose the production genesis host in a throwaway storage root and
    /// return `(module ids sorted, root hash hex, native module ids sorted)` —
    /// everything all three pins below need, so none has to keep its own copy
    /// of the construction.
    ///
    /// Production runs this root future on macOS's ~8 MiB process stack, and
    /// the reason to run the test twin on the same budget is to MATCH
    /// production, not to clear a measured bar: libtest's 2 MiB worker stack
    /// was observed too small for the full composition's debug build back when
    /// the set was 20 modules, and dropping to 19 was never re-measured.
    const GENESIS_TEST_STACK_BYTES: usize = 8 * 1024 * 1024;

    /// the genesis code set the pins compose over: the founding set the build
    /// staged beside this test executable — the committed components (the
    /// kernel fixtures pin the same bytes), read and hashed at test time,
    /// never embedded.
    fn fixture_genesis() -> GenesisModules {
        let dir = workspace_config::modules_dir().expect("the build stages the founding set");
        let hashes = noded::bundle::hash_bundle(&dir, &topology::TOPOLOGY.wasm_ids(PRODUCTION))
            .expect("founding set");
        GenesisModules {
            hashes,
            source: GenesisSource::FoundingSet(dir),
        }
    }

    /// a bare index store in `dir` for the genesis's guests to converge into.
    fn test_index(dir: &std::path::Path) -> indexer::IndexStore {
        indexer::IndexStore::open_bare(dir.join("index"), PRODUCTION).expect("open index")
    }

    fn genesis_facts() -> (Vec<String>, String, Vec<String>) {
        std::thread::Builder::new()
            .name("production-genesis-test".into())
            .stack_size(GENESIS_TEST_STACK_BYTES)
            .spawn(compose_genesis_facts)
            .expect("spawn production genesis test")
            .join()
            // Keep the original assertion or construction panic and location;
            // turning it into a generic join failure would hide the regression.
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    }

    fn compose_genesis_facts() -> (Vec<String>, String, Vec<String>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge_repo = dir.path().join("forge");
        let duckfs_dir = dir.path().join("duckfs");
        let cfg = commonware_runtime::tokio::Config::default()
            .with_storage_directory(dir.path().join("storage"));
        let executor = commonware_runtime::tokio::Runner::new(cfg);
        let index = test_index(dir.path());
        executor.start(|context| async move {
            let host = genesis_host(
                &context,
                &[],
                PIN_BINDINGS,
                NodeSubstrates {
                    forge_repo: &forge_repo,
                    duckfs_dir: &duckfs_dir,
                    blobs: blobstore::BlobHandle::default(),
                    index: &index,
                },
                &fixture_genesis(),
            )
            .await
            .expect("genesis host");
            // module_roots iterates the host's BTreeMap — sorted by id.
            let ids: Vec<String> = host.module_roots().into_iter().map(|(id, _)| id).collect();
            // a module with no code hash is one the binary compiled in rather
            // than one the registry can swap; `ids` is already sorted, so this is.
            let native = ids
                .iter()
                .filter(|id| host.module_code_hash(id).is_none())
                .cloned()
                .collect();
            (ids, hex(&host.root_hash()), native)
        })
    }

    /// a modules registry that activated `hello` on `first` at 10 and on
    /// `second` at 50 — the shape a restart finds after a live swap.
    fn registry_ahead(first: [u8; 32], second: [u8; 32]) -> modules::Modules {
        use modules::{Modules, ModulesMsg, encode_msg};
        use sdk::{Module as _, Origin};
        let member = vec![7u8; 32];
        let one_member = {
            let member = member.clone();
            move |req: &[u8]| -> Result<Vec<u8>, sdk::Error> {
                match valset::decode_query(req) {
                    Ok(valset::ValsetQuery::Validators) => Ok(valset::encode_reply(
                        &valset::ValsetReply::Validators(vec![member.clone()]),
                    )),
                    _ => Err(sdk::Error::QueryUnsupported),
                }
            }
        };
        let ctx = |origin: Origin, height: u64| {
            sdk_testkit::TestCtx::with_env(sdk::Env {
                height,
                consensus_time: 0,
                origin,
                me: "modules".into(),
                cause: sdk::Cause::Direct,
            })
            .on_query("valset", one_member.clone())
        };
        let msg = |m: ModulesMsg| sdk::Msg {
            target: "modules".into(),
            payload: encode_msg(&m),
        };
        let steps = [
            (
                Origin::System,
                10,
                ModulesMsg::RegisterModule {
                    module_id: "hello".into(),
                    code_hash: first.to_vec(),
                },
            ),
            (
                Origin::System,
                11,
                ModulesMsg::ScheduleSwap {
                    name: "next".into(),
                    module_id: "hello".into(),
                    activation_height: 50,
                    code_hash: second.to_vec(),
                },
            ),
            (
                Origin::External(member),
                12,
                ModulesMsg::SwapReady {
                    name: "next".into(),
                    module_id: "hello".into(),
                    code_hash: second.to_vec(),
                },
            ),
            (Origin::System, 50, ModulesMsg::Advance),
        ];
        let mut registry = Modules::new(
            "modules",
            Box::new(sdk_testkit::MemStore::new()),
            "valset",
            "governance",
        );
        futures::executor::block_on(async {
            for (origin, height, m) in steps {
                let mut ctx = ctx(origin, height);
                registry
                    .execute(&mut ctx, &msg(m))
                    .await
                    .expect("registry op");
                registry.commit_block().await.expect("commit");
            }
        });
        registry
    }

    /// the registry reopens AHEAD of the checkpoint: a module it swapped after
    /// the checkpoint is seated on the code that sealed the checkpoint's
    /// block, not the tip's active hash — replay's realization moves it
    /// forward from there. a checkpoint before the module's first activation
    /// seats that first code and starts it FRESH (the checkpoint predates the
    /// module: nothing to resume, and an `Err` there would refuse every
    /// restart between an activation and the next checkpoint); a checkpoint
    /// at or past it resumes.
    #[test]
    fn a_reopen_seats_the_code_at_its_height_and_starts_fresh_before_the_first_activation() {
        use noded::compose::{Seat, seat_at};
        use sdk::Module as _;
        let first = [1u8; 32];
        let second = [2u8; 32];
        let registry = registry_ahead(first, second);
        let req = modules::encode_query(&modules::ModulesQuery::ModuleStatus);
        // the registry serves its status through the host-routed query lane.
        let ctx = sdk_testkit::TestCtx::with_env(sdk::Env {
            height: 70,
            consensus_time: 0,
            origin: sdk::Origin::System,
            me: "modules".into(),
            cause: sdk::Cause::Direct,
        });
        let reply = futures::executor::block_on(registry.query_with(&ctx, &req)).expect("status");
        let Ok(modules::ModulesReply::ModuleStatus { modules: roster }) =
            modules::decode_reply(&reply)
        else {
            panic!("a status reply");
        };
        let hello = roster
            .iter()
            .find(|m| m.module_id == "hello")
            .expect("hello is registered");
        for (height, want) in [
            (5, Some((first, Seat::Fresh))),
            (20, Some((first, Seat::Resume))),
            (50, Some((second, Seat::Resume))),
            (70, Some((second, Seat::Resume))),
        ] {
            assert_eq!(seat_at(hello, height), want, "reopen at {height}");
        }
    }

    /// the registry ↔ topology parity pin. the composer builds genesis from
    /// the one `PRODUCTION` selection; this test pins the composed set to it,
    /// so a module composed by one path but missing from the selection the
    /// status/index surfaces open fails here instead of silently misreporting.
    #[test]
    fn genesis_registry_matches_production() {
        let (got, _root, _native) = genesis_facts();
        let mut want: Vec<String> = PRODUCTION.iter().map(|s| s.to_string()).collect();
        want.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn every_default_module_runs_wasm_including_the_registries() {
        let (_ids, _root, natives) = genesis_facts();
        assert!(
            natives.is_empty(),
            "the binary must construct no native modules: {natives:?}"
        );
    }

    /// a workspace genesis seeds every component into the blob store and
    /// itself as a chunk keyed by its pin; an absent file is REPORTED (a
    /// joiner fetches it) unless the blob store holds the chunk, which
    /// rewrites the file; a tampered file refuses by the whole-file hash; a
    /// file whose component disagrees with the descriptor refuses by module
    /// id.
    #[test]
    fn a_workspace_genesis_seeds_and_serves_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let genesis = Genesis {
            modules: vec![workspace_config::Artifact {
                id: "pages".into(),
                bytes: module_artifact::ModuleArtifact::component(b"pages-bytes".to_vec()).encode(),
            }],
        };
        let bytes = genesis.encode();
        let hash = workspace_config::genesis::sha256(&bytes);
        let want = genesis.module_hashes();
        let file = workspace_config::genesis_path(dir.path());

        let blobs = blobstore::BlobHandle::default();
        assert!(
            seed_workspace_genesis(&blobs, &file, &hash, &want)
                .expect("absence is reported")
                .is_none()
        );
        std::fs::write(&file, &bytes).expect("write");
        let loaded = seed_workspace_genesis(&blobs, &file, &hash, &want)
            .expect("seed")
            .expect("present");
        assert_eq!(loaded, genesis);
        assert!(blobs.has_chunk(&want["pages"]), "the component");
        assert!(
            blobs.has_chunk(&hash),
            "the genesis itself, for the next joiner"
        );

        // the file is the chunk's readable copy: lose it, and the seeded
        // store writes it back.
        std::fs::remove_file(&file).expect("remove");
        let rewritten = seed_workspace_genesis(&blobs, &file, &hash, &want)
            .expect("seed")
            .expect("rewritten from the blob store");
        assert_eq!(rewritten, genesis);
        assert_eq!(std::fs::read(&file).expect("the file is back"), bytes);

        let mut tampered = bytes.clone();
        tampered.push(0);
        std::fs::write(&file, &tampered).expect("write");
        let err = seed_workspace_genesis(&blobstore::BlobHandle::default(), &file, &hash, &want)
            .unwrap_err();
        assert!(err.contains("not the network's genesis"), "{err}");

        std::fs::write(&file, &bytes).expect("write");
        let mut wrong = want.clone();
        wrong.insert("pages".into(), [0u8; 32]);
        let err = seed_workspace_genesis(&blobstore::BlobHandle::default(), &file, &hash, &wrong)
            .unwrap_err();
        assert!(err.contains("module pages"), "{err}");
    }

    /// the dev shape's founding set: every file must hash to the descriptor's
    /// entry and land in the blob store; a mismatch or an absence refuses by
    /// module id and path.
    #[test]
    fn a_founding_set_seeds_every_component_or_refuses_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("pages.component.wasm"), b"pages-bytes").expect("write");
        let mut want = std::collections::BTreeMap::new();
        want.insert(
            "pages".to_string(),
            module_artifact::ModuleArtifact::component(b"pages-bytes".to_vec()).hash(),
        );
        let blobs = blobstore::BlobHandle::default();
        seed_founding_set(&blobs, dir.path(), &want).expect("seed");
        assert!(blobs.has_chunk(&want["pages"]));

        std::fs::write(dir.path().join("pages.component.wasm"), b"tampered").expect("write");
        let err =
            seed_founding_set(&blobstore::BlobHandle::default(), dir.path(), &want).unwrap_err();
        assert!(err.contains("module pages"), "{err}");

        std::fs::remove_file(dir.path().join("pages.component.wasm")).expect("remove");
        let err =
            seed_founding_set(&blobstore::BlobHandle::default(), dir.path(), &want).unwrap_err();
        assert!(err.contains("pages.component.wasm"), "{err}");
        // a chunk the store already holds needs no file at all.
        seed_founding_set(&blobs, dir.path(), &want).expect("seeded store");
    }

    /// THE consensus pin: the production genesis root hash is a constant.
    ///
    /// It is the only ABSOLUTE one in the tree, and until it existed every claim
    /// that "the root hash did not move" was relative and therefore weak.
    /// `bin/simnode/tests/topology_set.rs` pins the 15-module sim composition —
    /// which excludes `acl`, `governance`, `modules` and `valset`, and is not
    /// what a node runs. (Not a NATIVE composition, as this said for a while:
    /// simnode opens a `DirCodeSource` over the founding set the build staged
    /// beside it and composes through `noded::compose`, so every `SIM_BASE`
    /// id loads as a wasm component — which is why a rebuilt component moves
    /// that root.) And `git
    /// diff crates/modules/` on a committed tree is EMPTY BY CONSTRUCTION, so
    /// quoting it proves nothing at all. Neither would have noticed a module's
    /// bytes changing.
    ///
    /// ## the mechanism, because it surprises everyone once
    ///
    /// What this covers is wider than the module SET. The composer's modules registry
    /// seed commits each deployment hash — the descriptor's commitment — for
    /// every wasm tenant into the modules registry's MerkleStore, so each
    /// guest's CODE DIGEST is consensus state itself. That means a module's
    /// SOURCE is consensus-relevant the moment its component is rebuilt — even
    /// for a change that alters no behaviour, even a comment — and it means
    /// `make wasm-modules` can ship a seventeen-module flag day as a side effect
    /// of touching one guest. That is correct, and it is exactly the event that
    /// must never happen silently.
    ///
    /// ## when this fails
    ///
    /// You are in one of two situations and the message says so, because they
    /// need opposite responses:
    ///
    /// - **On purpose.** A module was added or removed, a guest was rebuilt, a
    ///   genesis-seeded record changed. Then this hash SHOULD move: update the
    ///   constant in the same commit, and say in the commit message which change
    ///   moved it. A flag day is cheap — there is no live chain — but it has to
    ///   be a stated act.
    /// - **By accident.** You did not mean to touch consensus, and you did. The
    ///   usual cause is a rebuilt `component.wasm` riding along in the diff.
    #[test]
    fn production_genesis_root_hash_is_pinned() {
        let (_ids, root, _native) = genesis_facts();
        assert_eq!(
            root, GENESIS_ROOT_HASH,
            "the production genesis root hash MOVED.\n\
             Every node computes this at block zero, so a network whose members \
             do not all agree on it forks at genesis.\n\
             \n\
             DID YOU MEAN TO? A module added/removed, a guest rebuilt, a \
             genesis-seeded record changed — then yes, and this is a deliberate \
             flag day: set GENESIS_ROOT_HASH to {root} in the SAME commit as the \
             change that moved it, and name that change in the commit message.\n\
             \n\
             DID YOU NOT? Then you have moved consensus by accident. Look for a \
             rebuilt component.wasm in your diff — a guest's code digest is \
             consensus state, so `make wasm-modules` moves this hash even when \
             the source change was cosmetic:\n\
             \x20 git diff origin/dev --name-only crates/modules/ crates/guests/ crates/examples/"
        );
    }

    /// `sync_all_modules` re-enters forever on a resident whose boundary sync
    /// keeps failing (`replica/park.rs`'s retry loop only bails when NOT
    /// `resident_standing`), so a label leaked per attempt grows without
    /// bound. Two lookups over the same module set must return the SAME
    /// leaked string — i.e. add nothing new to the interner — not a fresh
    /// leak each time.
    #[test]
    fn intern_label_reuses_existing_leak_for_repeat_module_ids() {
        let module_set = ["directory", "governance", "modules", "files"];

        let first_attempt: Vec<&'static str> = module_set
            .iter()
            .map(|module| intern_label(module))
            .collect();
        let second_attempt: Vec<&'static str> = module_set
            .iter()
            .map(|module| intern_label(module))
            .collect();

        for (a, b) in first_attempt.iter().zip(second_attempt.iter()) {
            assert!(
                std::ptr::eq(*a, *b),
                "a repeat label lookup for the same module id leaked a new string \
                 instead of returning the interned one"
            );
        }
    }

    /// a source serving a boundary it cannot transfer is refused BEFORE the
    /// sync spends a genesis fetch, an object possession loop and every store
    /// rebuild on it — and the refusal names the module and carries the
    /// rotation's reason token.
    #[test]
    fn a_degraded_source_module_refuses_the_boundary() {
        let entry = |id: &str, kind| statesync::ManifestEntry {
            module_id: id.into(),
            root: StateRoot([1u8; 32]),
            code_hash: None,
            kind,
            resolver_target: None,
        };
        let manifest = |entries| statesync::Manifest {
            height: 9,
            root_hash: StateRoot([2u8; 32]),
            epoch: 1,
            view_base: 0,
            participants: vec![],
            residents: vec![],
            floor_cert: None,
            entries,
            applied_frames: vec![],
            pending_cutover_view: None,
        };
        let degraded = undeliverable_boundary(&manifest(vec![
            entry("valset", statesync::PayloadKind::Resolver),
            entry("kv", statesync::PayloadKind::Unsupported),
        ]))
        .expect("an Unsupported entry refuses the boundary");
        assert_eq!(degraded.reason(), "source_degraded_module");
        assert!(
            degraded.to_string().contains("kv"),
            "the refusal names the module: {degraded}"
        );
        assert!(
            undeliverable_boundary(&manifest(vec![
                entry("valset", statesync::PayloadKind::Resolver),
                entry("kv", statesync::PayloadKind::Snapshot),
            ]))
            .is_none()
        );
    }
}
