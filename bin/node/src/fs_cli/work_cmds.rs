//! the working-copy loop: checkout / status / commit / pin, and the one-file
//! `put`. these operate on a local checkout dir plus its `.duckfs` index; the
//! node address comes from the shared [`crate::cli_args::NodeAddr`] ladder,
//! which for verbs running inside a checkout takes the index's recorded node
//! url as its context rung.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use duckfs_client::api::ApiError;
use duckfs_client::checkout::{CheckoutError, CheckoutOptions, checkout_with};
use duckfs_client::commit::{CommitError, CommitOptions, commit_with};
use duckfs_client::http::HttpNode;
use duckfs_client::index::Index;

use crate::fs_cli::args::{CliError, NodeAddr, api_err, resolve_node};
use crate::fs_cli::{CheckoutArgs, CommitArgs, PinArgs, PutArgs, StatusArgs, UnpinArgs};

/// resolve the node for a verb running inside `dir`: the shared addressing
/// ladder, with this checkout's `.duckfs` index as the ambient context rung —
/// below what the operator stated, above the registry inference. A checkout
/// records the node it came FROM, which beats "the one workspace registered on
/// this box".
fn url_for_dir(addr: &NodeAddr, dir: &Path) -> Result<String, CliError> {
    let recorded = || Index::load(dir).ok().map(|index| index.node);
    addr.resolve_with(recorded).map_err(CliError::usage)
}

/// a node whose WRITES carry the acting person's signature.
///
/// Every mutating duckfs route refuses a request that proves nothing, and this
/// verb is a person's: what it stages and commits is charged to the key signing
/// here — the commit's author, the `/home/<owner>/**` authority, and the
/// staging quota (#1312). Opening the key costs one password prompt per verb,
/// which is why it happens ONCE, before the walk, and the closure reuses the
/// opened key for every chunk.
///
fn signing_node(
    addr: &NodeAddr,
    dir: &Path,
    key: Option<PathBuf>,
    trust_node: bool,
) -> Result<HttpNode, CliError> {
    let url = url_for_dir(addr, dir)?;
    let ctx = crate::cred_cli::VerbCtx {
        addr: addr.clone(),
        key,
    };
    let key_path = ctx
        .key_path()
        .map_err(|e| CliError::failed(e.to_string()))?;
    let _node_key = crate::node_http::pinned_node_key(&key_path, &url, trust_node)
        .map_err(|error| CliError::failed(error.to_string()))?;
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let signer = crate::userkey_cli::load_user_signer_for(&url, &key_path, &mut stdin)
        .map_err(|e| CliError::failed(e.to_string()))?;
    Ok(
        HttpNode::new(url).with_frame_signer(Arc::new(move |target, payload| {
            crate::userkey_cli::user_frame(&signer, target, payload)
        })),
    )
}

/// a node failure goes through [`api_err`], the mapping every other verb uses,
/// so a checkout refusal keeps both halves; the rest are this side's own.
fn checkout_err(e: CheckoutError) -> CliError {
    match e {
        CheckoutError::Api(e) => api_err(e),
        other => CliError::failed(other.to_string()),
    }
}

/// a commit failure other than the conflict report `commit` prints itself. a
/// refusal keeps both halves, and a node that stopped answering mid-commit is
/// told in the same sentence, as [`api_err`] does.
fn commit_err(e: CommitError) -> CliError {
    match e {
        CommitError::Nothing => CliError::failed("nothing to commit (the working copy is clean)"),
        CommitError::Rejected { reason, sentence } => CliError::refused(reason, sentence),
        CommitError::Unreachable { base } => api_err(ApiError::Unreachable { base }),
        other => CliError::failed(other.to_string()),
    }
}

/// `checkout <prefix> <dir> [--snapshot S]` — materialize the subtree and write
/// the `.duckfs` index recording the node it was checked out from.
pub fn checkout(args: CheckoutArgs) -> Result<(), CliError> {
    // a fresh checkout has no index yet, so the node MUST be explicit.
    let url = resolve_node(&args.addr)?;
    let node = HttpNode::new(url.clone());
    let snapshot = args.snapshot.as_deref();
    let opts = CheckoutOptions {
        node_url: url,
        ..Default::default()
    };
    let index = checkout_with(&node, Path::new(&args.dir), &args.prefix, snapshot, &opts)
        .map_err(checkout_err)?;
    let base = index.base_snapshot.as_deref().unwrap_or("(empty tree)");
    println!("checked out {} at {base} into {}", args.prefix, args.dir);
    Ok(())
}

/// `status [dir] [--path P]...` (default `.`) — one `A|M|D\tpath` line per
/// change, exit 1 when dirty (script-friendly: exit code IS the clean/dirty
/// signal). `--path` reports what the same pathspec would commit.
pub fn status(args: StatusArgs) -> Result<(), CliError> {
    let dir = args.dir.as_deref().unwrap_or(".");
    let dirp = Path::new(dir);
    let st = duckfs_client::status::status(dirp).map_err(|e| CliError::failed(e.to_string()))?;
    let st = match args.paths.is_empty() {
        true => st,
        // the pathspec is written against the checkout, so it needs the prefix
        // the index recorded.
        false => {
            let index = Index::load(dirp).map_err(|e| CliError::failed(e.to_string()))?;
            st.select(&args.paths, &index.prefix)
        }
    };

    for e in &st.added {
        println!("A\t{}", e.path);
    }
    for e in &st.modified {
        println!("M\t{}", e.path);
    }
    for path in &st.removed {
        println!("D\t{path}");
    }

    if st.clean {
        Ok(())
    } else {
        // the changes are already printed; exit 1 with no extra error line.
        Err(CliError::silent(1))
    }
}

/// `commit [dir] --message <m> [--no-rebase] [--path P]...` — commit the working
/// copy, or the part of it the pathspec selects. prints the new snapshot id; a
/// conflict prints the report to stderr and exits 2.
pub fn commit(args: CommitArgs) -> Result<(), CliError> {
    let dir = args.dir.as_deref().unwrap_or(".");
    let dirp = Path::new(dir);
    let node = signing_node(&args.addr, dirp, args.key, args.trust_node)?;
    let opts = CommitOptions {
        auto_rebase: !args.no_rebase,
        paths: args.paths,
    };

    match commit_with(&node, dirp, &args.message, &opts) {
        Ok(summary) => {
            println!("{}", summary.snapshot);
            if summary.rebased {
                eprintln!("ducktape fs: auto-rebased onto the current head before committing");
            }
            Ok(())
        }
        Err(CommitError::Conflict(report)) => {
            eprintln!("ducktape fs: commit conflict");
            eprintln!("  base: {}", report.base.as_deref().unwrap_or("(none)"));
            eprintln!("  head: {}", report.head.as_deref().unwrap_or("(none)"));
            for path in &report.clashing {
                eprintln!("  clashing: {path}");
            }
            if !report.remedy.is_empty() {
                eprintln!("  remedy: {}", report.remedy);
            }
            Err(CliError::silent(2))
        }
        Err(e) => Err(commit_err(e)),
    }
}

/// `put <local> <path> [--message M]` — write one local file to an absolute
/// duckfs path in ONE commit, with no checkout: a file within the inline
/// budget rides inside the commit op; a larger one is staged a 1 MiB chunk
/// per op (a chunk still staged from an interrupted run is skipped, as
/// `commit` does — a commit consumes its staging, so a re-put stages afresh)
/// and then named by one commit. Every operation uses the generic signed-frame
/// transport, including each bounded chunk of a release archive.
/// prints the new snapshot id.
pub fn put(args: PutArgs) -> Result<(), CliError> {
    use base64::Engine as _;
    use duckfs_client::api::NodeApi;
    use duckfs_client::chunk::chunk_ids;
    use duckfs_core::{CHUNK_SIZE, Change, Content, MAX_INLINE_COMMIT_BYTES, MAX_SYNC_IDS, to_hex};

    let bytes = std::fs::read(&args.local)
        .map_err(|e| CliError::failed(format!("read {}: {e}", args.local.display())))?;
    let node = signing_node(&args.addr, Path::new("."), args.key, args.trust_node)?;

    let rides_inline = bytes.len() <= MAX_INLINE_COMMIT_BYTES;
    let content = match rides_inline {
        true => Content::Inline {
            b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        },
        false => {
            let hexes: Vec<String> = chunk_ids(&bytes).iter().map(|id| to_hex(id)).collect();
            let slices: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE as usize).collect();
            let mut staged = 0usize;
            for (batch_index, batch) in hexes.chunks(MAX_SYNC_IDS).enumerate() {
                let present = node.has_chunks(batch).map_err(api_err)?;
                for (offset, (hex, present)) in batch.iter().zip(present).enumerate() {
                    if present {
                        continue;
                    }
                    let index = batch_index * MAX_SYNC_IDS + offset;
                    let digest = node.stage_chunk(slices[index]).map_err(api_err)?;
                    let landed_as_named = digest == *hex;
                    if !landed_as_named {
                        return Err(CliError::failed(format!(
                            "the node staged chunk {index} as {digest}, expected {hex}"
                        )));
                    }
                    staged += 1;
                }
            }
            eprintln!(
                "ducktape fs: staged {staged} of {} chunks ({} bytes)",
                hexes.len(),
                bytes.len()
            );
            Content::Chunks {
                size: bytes.len() as u64,
                chunks: hexes,
            }
        }
    };

    let head = node.refs().map_err(api_err)?.head;
    let message = args.message.unwrap_or_else(|| format!("put {}", args.path));
    let change = Change::Put {
        path: args.path.clone(),
        exec: false,
        meta: BTreeMap::new(),
        content,
    };
    let receipt = node
        .commit(head.as_deref(), &message, vec![change])
        .map_err(api_err)?;
    let snapshot = snapshot_of(&node, receipt.height, &message)?;
    println!("{snapshot}");
    Ok(())
}

/// the snapshot id the commit at `height` with `message` produced. a block
/// can hold several members' commits, so the height alone is ambiguous; the
/// message is this verb's own and the newest-first window is short.
fn snapshot_of(node: &HttpNode, height: u64, message: &str) -> Result<String, CliError> {
    use duckfs_client::api::NodeApi;
    use duckfs_core::MAX_PAGE;

    let window = node.history(MAX_PAGE).map_err(api_err)?;
    let ours = window
        .iter()
        .find(|entry| entry.height == height && entry.message == message)
        .map(|entry| entry.id.clone());
    match ours {
        Some(id) => Ok(id),
        // THIS side could not name what it just wrote; the node refused
        // nothing, so the class is the CLI's own.
        None => Err(CliError::refused(
            "snapshot_unresolved",
            format!("the commit landed at height {height} but the history window does not show it"),
        )),
    }
}

/// `pin <snapshot> <name>` — pin a snapshot by name so gc keeps it reachable.
pub fn pin(args: PinArgs) -> Result<(), CliError> {
    use duckfs_client::api::NodeApi;

    // pin runs against a node directly (default `.` so a checkout's index can
    // supply the node, but `--node`/env win).
    let node = signing_node(&args.addr, Path::new("."), args.key, args.trust_node)?;
    node.pin(&args.snapshot, &args.name).map_err(api_err)?;
    println!("pinned {} as {}", args.snapshot, args.name);
    Ok(())
}

/// `unpin <name>` — release a pin so gc can reclaim it once nothing else roots
/// it. any signer releases any pin.
pub fn unpin(args: UnpinArgs) -> Result<(), CliError> {
    use duckfs_client::api::NodeApi;

    let node = signing_node(&args.addr, Path::new("."), args.key, args.trust_node)?;
    node.unpin(&args.name).map_err(api_err)?;
    println!("unpinned {}", args.name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_cli::args::Message;

    fn refused(reason: &str, sentence: &str) -> ApiError {
        ApiError::Rejected {
            reason: reason.to_string(),
            sentence: sentence.to_string(),
        }
    }

    /// `checkout` and `commit` refuse in the line every other verb does, and
    /// keep both halves on the way: neither flattens a refusal through its
    /// error's `Display` into one string.
    #[test]
    fn checkout_and_commit_refusals_keep_both_halves() {
        let rows = [
            (
                checkout_err(CheckoutError::Api(refused(
                    "files_query",
                    "files: path not found",
                ))),
                "files_query",
                "files: path not found",
            ),
            (
                commit_err(CommitError::from(refused(
                    "files_commit",
                    "files: path must be absolute (start with '/')",
                ))),
                "files_commit",
                "files: path must be absolute (start with '/')",
            ),
        ];
        for (error, reason, sentence) in rows {
            assert_eq!(error.code, 1);
            assert_eq!(
                error.message,
                Message::Refused {
                    reason: reason.to_string(),
                    sentence: sentence.to_string(),
                }
            );
            assert_eq!(error.line(), Some(format!("{sentence} [{reason}]")));
        }
    }
}
