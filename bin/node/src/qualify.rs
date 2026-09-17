//! `ducktape node qualify` — can THIS binary run THIS workspace?
//!
//! The question a release launcher asks a staged binary before it flips to
//! it, and the only honest way to ask it: reopen the workspace's checkpoint
//! and recompose the state it committed, which is exactly the restart path a
//! live node takes. A binary whose module set, WIT world or state layout
//! disagrees with what the network committed cannot reach the same root, and
//! it says so here — with the old binary still installed and the node still
//! runnable — instead of at the boot after a flip, where the only way out is
//! a rollback.
//!
//! It opens no listener, joins no mesh and runs no consensus. It DOES open
//! the workspace's stores, so the node must be stopped: a launcher runs this
//! between stopping the node and flipping.
//!
//! Program output, not logging: one snake_case reason on stdout (`ok` plus
//! the root hash on success), and the exit status is the answer.

use commonware_runtime::{Runner as _, Supervisor as _};
use recovery::Recovery;

use crate::cli_args::SelectorArgs;
use crate::config;
use crate::host_state::{NetworkBindings, NodeSubstrates, restore_host};
use crate::util::hex;

type CommandResult = Result<(), Box<dyn std::error::Error>>;

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

pub(crate) fn run(args: SelectorArgs) -> CommandResult {
    let cfg_path = args.selector.config_path()?;
    let resolved = config::resolve(&cfg_path)?;
    match reopen(resolved) {
        Ok(root) => {
            println!("ok\t{root}");
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
        let recovery = Recovery::open(context.child("recovery"))
            .await
            .map_err(|error| Unqualified::new("recovery_unopenable", error.to_string()))?;
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
        let committed = manifest.root_hash;
        let host = restore_host(
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
        let reached = host.root_hash();
        let agrees = reached == committed;
        if !agrees {
            return Err(Unqualified::new(
                "root_hash_diverged",
                format!(
                    "this binary recomposed {} where the checkpoint committed {}",
                    hex(&reached),
                    hex(&committed)
                ),
            ));
        }
        Ok(hex(&reached))
    })
}
