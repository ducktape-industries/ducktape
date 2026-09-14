//! node configuration, in two halves.
//!
//! **Reading and writing a workspace** — the identity file, `network.toml`,
//! `node.toml`, the invite codec, and the join ceremony — lives in the
//! `workspace-config` crate, because two very different programs do it: this
//! CLI and the desktop app. It left this binary when the app's only way to join
//! a network was to spawn `ducktape node join` and diff the workspace registry
//! around the call to find out which directory the child had created.
//!
//! **Resolving those files into a runnable daemon** stays here, in
//! [`resolve`]. Only the thing that boots a node needs to; it is where the
//! `[sandbox]` table becomes a live backend gated on a compute grant, and where
//! the dev-seed shape is folded into the same runnable form.
//!
//! Everything is re-exported flat, so `config::<anything>` resolves exactly as
//! it did when all five files sat in this directory.

mod resolve;

pub use resolve::*;
pub use workspace_config::*;

/// compile-validate every deployment `genesis` carries — the same check a
/// validator runs before arming a live swap (`noded::compose::validate_deployment`:
/// `declared_shape` + `check_realizable` + the mapper's `on_apply`/`query`
/// export for a module, the view ABI for a view-only entry) — before a
/// founder writes a single byte. Discovery
/// (`Genesis::compose`) only checks filenames, ids, and the Borsh/mapper
/// framing; this is the first point that actually loads each component and
/// mapper through wasmtime, so a zero-byte or truncated artifact refuses
/// `node init` instead of minting an unbootable network.
///
/// called only from `cmd_init`, not from `resolve_dev_shape`: the dev shape
/// never writes a genesis or descriptor file (its `GenesisSource::FoundingSet`
/// is re-hashed from the staged directory on every resolve, i.e. every dev
/// boot), and `compose()` a moment later already compiles the same set again
/// — validating here too would pay a full module-set compile on every boot
/// for nothing durable to protect.
pub fn validate_founding_set(
    source: &std::path::Path,
    genesis: &workspace_config::Genesis,
) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let scratch = std::env::temp_dir().join(format!(
        "ducktape-init-validate-{}-{nanos}",
        std::process::id()
    ));
    // no module id needs its own database here: `validate_deployment` only
    // ever touches the shared blocks database (compiling bytes), never a
    // per-module one.
    let index = indexer::IndexStore::open_bare(&scratch, &[])
        .map_err(|e| format!("open validation store at {}: {e}", scratch.display()))?;
    // at genesis an entry's kind IS its frame's tag (`compose::genesis_seeds`
    // seeds the registry the same way), so each founding file is validated
    // as what it says it is: a module through its core, a view through the
    // view ABI alone.
    let result = genesis.modules.iter().try_for_each(|module| {
        let kind = noded::compose::artifact_kind(&module.bytes)
            .map_err(|error| format!("{} {}: {error}", source.display(), module.id))?;
        let file = match kind {
            modules::Kind::Module => workspace_config::component_path(source, &module.id),
            modules::Kind::View => source.join(format!("{}.view.wasm", module.id)),
        };
        noded::compose::validate_deployment(&module.id, kind, &module.bytes, &index)
            .map_err(|error| format!("{}: {error}", file.display()))
    });
    drop(index);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}
