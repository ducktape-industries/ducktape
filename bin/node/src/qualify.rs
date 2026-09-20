//! `ducktape node qualify` — can THIS binary run THIS workspace?
//!
//! The question a release launcher asks a staged binary before it flips to
//! it, and the only honest way to ask it: reopen the workspace's checkpoint,
//! roll it forward through the journal suffix and verify the root consensus
//! sealed at the tip, which is exactly the restart path a live node takes. A
//! binary whose module set, WIT world or state layout disagrees with what the
//! network committed cannot reach the same root, and it says so here — with
//! the old binary still installed and the node still runnable — instead of at
//! the boot after a flip, where the only way out is a rollback.
//!
//! It opens no listener, joins no mesh and runs no consensus. It DOES open
//! the workspace's stores, so the node must be stopped: a launcher runs this
//! between stopping the node and flipping. And it DOES what that restart does
//! to them: a torn journal tail is rewound, and a block the stop caught
//! mid-apply is re-applied and sealed — the same writes the next boot of
//! either binary makes first.
//!
//! `--compose-only` asks the LINKER half of that question and nothing else:
//! do the components this network is running load against this binary's
//! `ducktape:module/host` world? It reads the roster off the live node's rpc
//! and each component out of the blobstore's immutable content-addressed
//! files, so it opens no store, replays no journal and takes no lock — it
//! runs BESIDE a node that never notices. That is the check a designation
//! can afford before it is proposed. It says nothing about state layout;
//! the full reopen above stays the gate for that.
//!
//! Program output, not logging: one snake_case reason on stdout (`ok` plus
//! the root hash on success), and the exit status is the answer.

use std::collections::BTreeMap;
use std::path::Path;

use commonware_runtime::{Runner as _, Supervisor as _};
use recovery::Recovery;
use sha2::Digest as _;

use crate::cli_args::QualifyArgs;
use crate::config;
use crate::host_state::{BlobCodeSource, NetworkBindings, NodeSubstrates, restore_host};
use crate::util::hex;

type CommandResult = Result<(), Box<dyn std::error::Error>>;

/// which question this run asks of the binary running it.
enum Question {
    /// the whole restart path: reopen the workspace's checkpoint, roll it
    /// forward through the journal and verify the sealed tip. needs the node
    /// STOPPED.
    Checkpoint,
    /// the linker alone, over the module set the network is running. lock-free
    /// and answerable beside a live node.
    ComposeOnly,
}

/// Why a binary cannot run this workspace. A stable snake_case token: the
/// launcher logs it as `reason` and the machine carries it in
/// `Event::QualifyFailed`.
struct Unqualified {
    reason: &'static str,
    detail: String,
}

impl Unqualified {
    fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Unqualified {
            reason,
            detail: detail.into(),
        }
    }
}

pub(crate) fn run(args: QualifyArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    let question = match args.compose_only {
        true => Question::ComposeOnly,
        false => Question::Checkpoint,
    };
    let answer = match question {
        Question::Checkpoint => reopen(resolved),
        Question::ComposeOnly => links(&resolved),
    };
    match answer {
        Ok(verdict) => {
            println!("ok\t{verdict}");
            Ok(())
        }
        Err(refusal) => {
            // stdout carries the reason because that is what the launcher
            // reads; stderr carries the detail for a person.
            println!("{}", refusal.reason);
            eprintln!("{}: {}", refusal.reason, refusal.detail);
            std::process::exit(1);
        }
    }
}

/// Reopen the checkpoint and recompose it. Returns the root hash reached,
/// which must be the one the checkpoint committed.
fn reopen(resolved: config::Resolved) -> Result<String, Unqualified> {
    let storage = resolved.service.storage_dir.clone();
    let namespace = resolved.namespace.clone();
    let identity_chain_id = resolved.service.chain_id.clone();
    let genesis = resolved.genesis;

    let index = noded::open_index_store::<&str>(&storage, &[])
        .map_err(|error| Unqualified::new("index_unopenable", error))?;
    let blobs = blobstore::BlobHandle::persistent(storage.join("blobstore"))
        .map_err(|error| Unqualified::new("blobstore_unopenable", error.to_string()))?;
    let forge_repo = storage.join("forge-repo");
    let duckfs_dir = storage.join("duckfs");

    // commonware's runtime owns its own tokio runtime and roots every store
    // it opens at this directory — the same one the node runs over.
    let config = commonware_runtime::tokio::Config::default().with_storage_directory(&storage);
    let runner = commonware_runtime::tokio::Runner::new(config);
    runner.start(|context| async move {
        let mut recovery = Recovery::open(context.child("recovery"))
            .await
            .map_err(|error| Unqualified::new("recovery_unopenable", error.to_string()))?;
        // the source the node's own boot replays through: a code swap in the
        // journal suffix realizes from this workspace's content-addressed
        // store.
        recovery.set_code_source(std::sync::Arc::new(BlobCodeSource(std::sync::Arc::new(
            blobs.clone(),
        ))));
        let manifest = recovery
            .manifest()
            .map_err(|error| Unqualified::new("checkpoint_damaged", error.to_string()))?;
        // Nothing committed means nothing to reopen. A launcher only ever
        // flips a workspace a node has run, so this is a misuse, not a
        // verdict about the binary.
        let manifest = manifest.ok_or_else(|| {
            Unqualified::new(
                "no_checkpoint",
                format!("{} holds no checkpoint to reopen", storage.display()),
            )
        })?;
        let mut host = restore_host(
            &context,
            &manifest,
            NetworkBindings {
                invite: &namespace,
                identity_chain_id: &identity_chain_id,
            },
            NodeSubstrates {
                forge_repo: &forge_repo,
                duckfs_dir: &duckfs_dir,
                blobs,
                index: &index,
            },
            &genesis,
        )
        .await
        .map_err(|error| Unqualified::new("checkpoint_unrestorable", error))?;
        // the rest of the restart path: roll forward through the journal
        // suffix. a per-block-durable module commits to its own disk every
        // block while the checkpoint persists on a cadence, so a node stopped
        // with no final checkpoint — a resident on SIGTERM, any node killed —
        // leaves those modules ahead of the manifest's root. comparing the
        // restore to that root refuses every such workspace; recovery floors
        // them, re-applies the rest, and verifies the tip's sealed root.
        let recovered = recovery
            .recover(&mut host, &manifest)
            .await
            .map_err(unrecovered)?;
        Ok(hex(&recovered.root_hash))
    })
}

/// Recovery's refusal as a qualify reason. `Verify` is this binary reaching a
/// state consensus never sealed; every other arm is about the workspace.
fn unrecovered(error: recovery::Error) -> Unqualified {
    let reason = match &error {
        recovery::Error::Verify(_) => "root_hash_diverged",
        recovery::Error::Storage(_)
        | recovery::Error::Corrupt(_)
        | recovery::Error::Torn(_)
        | recovery::Error::FieldOverCap(_)
        | recovery::Error::RangePruned { .. } => "journal_unrecoverable",
    };
    Unqualified::new(reason, error.to_string())
}

/// Load the module set the network is RUNNING against this binary's wasm
/// world. Returns how many components linked.
fn links(resolved: &config::Resolved) -> Result<String, Unqualified> {
    let rpc = resolved.rpc_listen.as_deref().ok_or_else(|| {
        Unqualified::new(
            "rpc_unset",
            "node qualify --compose-only reads the running node's module roster — set \
             `rpc_listen` in node.toml",
        )
    })?;
    let roster = crate::module_cli::read_module_status(rpc)
        .map_err(|error| Unqualified::new("roster_unreadable", error))?;
    let running = running_code(&roster);
    // A roster with no running component is not a verdict about this binary:
    // either the registry answered with nothing or every module is an
    // admission that has not reached its boundary, and linking zero
    // components would report "yes" to a question never asked.
    if running.is_empty() {
        return Err(Unqualified::new(
            "no_running_modules",
            "the modules registry names no activated component to link against",
        ));
    }
    let blobstore = resolved.service.storage_dir.join("blobstore");
    for (id, code_hash) in &running {
        let bytes = blob(&blobstore, code_hash)
            .map_err(|error| Unqualified::new("module_code_absent", error))?;
        noded::compose::load(id, &bytes)
            .map_err(|error| Unqualified::new("modules_unlinkable", error))?;
    }
    Ok(format!("{} modules link", running.len()))
}

/// what the registry says each module is executing right now: its last
/// activation's hash. A view or a plane entry has no component to link here,
/// and an admission that has not reached its boundary yet has no active hash
/// at all.
fn running_code(roster: &[modules::ModuleCode]) -> BTreeMap<String, [u8; 32]> {
    roster
        .iter()
        .filter(|entry| match entry.kind {
            modules::Kind::Module => true,
            modules::Kind::View | modules::Kind::Plane => false,
        })
        .filter_map(|entry| {
            let hash: [u8; 32] = entry.active_code_hash.as_slice().try_into().ok()?;
            Some((entry.module_id.clone(), hash))
        })
        .collect()
}

/// one component out of the node's blob plane WITHOUT opening it. A blob is
/// an immutable file named by the hex of its own sha256, written through
/// under a temporary name and renamed into place, so reading one beside a
/// running node needs no lock and cannot disturb what that node is doing —
/// which is the whole reason this mode exists. The bytes are re-hashed here
/// exactly as the node's own code source re-hashes them: a truncated or
/// swapped file is an absence, never code.
fn blob(blobstore: &Path, code_hash: &[u8; 32]) -> Result<Vec<u8>, String> {
    let path = blobstore.join(noded::hex_bytes(code_hash));
    let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let matches_hash = sha2::Sha256::digest(&bytes)[..] == code_hash[..];
    if !matches_hash {
        return Err(format!(
            "{} does not hash to the code it is named for",
            path.display()
        ));
    }
    Ok(bytes)
}
