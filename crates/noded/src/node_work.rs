//! A block-driven interpreter of the node-work protocol. Consensus modules
//! supply policy; this bridge owns only local blob I/O and node-key submission.
use commonware_cryptography::Signer as _;
use futures::SinkExt as _;
use futures::channel::oneshot;
use node_work::{Directive, ForgeBlob, Query, Reply, Submission};
use sha2::Digest as _;

use crate::{NodeCommand, NodeHandle};

async fn query(
    handle: &NodeHandle,
    source: &str,
    node_key: Vec<u8>,
) -> Result<Option<Directive>, String> {
    let (reply, rx) = oneshot::channel();
    let status = handle.status.current();
    handle
        .command_sender()
        .send(NodeCommand::Query {
            target: source.into(),
            req: sdk::wire::encode(&Query::NodeWork {
                node_key,
                height: status.height,
                consensus_time: status.consensus_time,
            }),
            reply,
        })
        .await
        .map_err(|_| "node command lane closed".to_string())?;
    let bytes = rx
        .await
        .map_err(|_| "node dropped work query".to_string())??;
    let Reply::NodeWork(directive) = sdk::wire::decode(&bytes)?;
    Ok(directive)
}

async fn submit(handle: &NodeHandle, message: Submission) -> Result<(), String> {
    let (reply, rx) = oneshot::channel();
    let key = handle
        .node_signer
        .as_ref()
        .ok_or("node signer is unavailable")?
        .public_key()
        .as_ref()
        .to_vec();
    handle
        .command_sender()
        .send(NodeCommand::Submit {
            required_blob: None,
            target: message.target,
            payload: message.payload,
            origin: key,
            reply,
        })
        .await
        .map_err(|_| "node command lane closed".to_string())?;
    rx.await
        .map_err(|_| "node dropped work submission".to_string())??;
    Ok(())
}

#[derive(Debug)]
enum BlobError {
    Unavailable(String),
    Invalid(String),
}

fn blob_entry<'repo>(
    repo: &'repo git2::Repository,
    mut tree: git2::Tree<'repo>,
    path: &str,
) -> Result<git2::TreeEntry<'static>, BlobError> {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        let entry = tree
            .get_name(part)
            .map(|entry| entry.to_owned())
            .ok_or_else(|| {
                BlobError::Invalid("blob file is absent from the committed tree".into())
            })?;
        let last = parts.peek().is_none();
        if last {
            return Ok(entry);
        }
        let directory = entry.kind() == Some(git2::ObjectType::Tree);
        if !directory {
            return Err(BlobError::Invalid(
                "blob path crosses a non-directory".into(),
            ));
        }
        // A replicated commit can arrive before all of its tree objects. That
        // is retryable local absence, not evidence that its path is invalid.
        tree = repo
            .find_tree(entry.id())
            .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    }
    Err(BlobError::Invalid("blob path is empty".into()))
}

fn read_blob(base: &std::path::Path, source: &ForgeBlob) -> Result<Vec<u8>, BlobError> {
    let exact =
        source.commit.len() == 40 && source.commit.bytes().all(|byte| byte.is_ascii_hexdigit());
    let valid = exact && node_work::relative_path(&source.path);
    if !valid {
        return Err(BlobError::Invalid(
            "blob requires an exact commit and a relative file path".into(),
        ));
    }
    let name =
        forge::norm_repo(&source.repo).map_err(|error| BlobError::Invalid(error.to_string()))?;
    let repo = git2::Repository::open(base.join(name))
        .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    let oid = git2::Oid::from_str(&source.commit)
        .map_err(|error| BlobError::Invalid(error.to_string()))?;
    let commit = repo
        .find_commit(oid)
        .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    let tree = commit
        .tree()
        .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    let entry = blob_entry(&repo, tree, &source.path)?;
    let regular = matches!(entry.filemode(), 0o100644 | 0o100755);
    if !regular {
        return Err(BlobError::Invalid("blob must be a regular git file".into()));
    }
    let odb = repo
        .odb()
        .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    let (size, kind) = odb
        .read_header(entry.id())
        .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    let bounded = kind == git2::ObjectType::Blob && size <= node_work::MAX_STAGED_BLOB_BYTES;
    if !bounded {
        return Err(BlobError::Invalid(
            "blob exceeds the staging byte bound".into(),
        ));
    }
    let blob = repo
        .find_blob(entry.id())
        .map_err(|error| BlobError::Unavailable(error.to_string()))?;
    let bytes = blob.content();
    let hash: [u8; 32] = sha2::Sha256::digest(bytes).into();
    if hash != source.hash {
        return Err(BlobError::Invalid(
            "forge file does not match the requested blob hash".into(),
        ));
    }
    Ok(bytes.to_vec())
}

async fn stage(
    handle: &NodeHandle,
    source: ForgeBlob,
    on_ready: Submission,
    on_invalid: Submission,
) -> Result<(), String> {
    let base = handle
        .forge_repo
        .clone()
        .ok_or("forge storage is unavailable")?;
    let blobs = handle.blobs.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        // Re-reading the immutable source also validates paths on a cache hit.
        let bytes = read_blob(&base, &source)?;
        let mut staged = blobs
            .stage(source.hash, bytes.len() as u64)
            .map_err(|error| BlobError::Unavailable(error.to_string()))?;
        let offset = staged.offset() as usize;
        staged
            .append(&bytes[offset..])
            .map_err(|error| BlobError::Unavailable(error.to_string()))?;
        staged
            .finish()
            .map_err(|error| BlobError::Unavailable(error.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?;
    match outcome {
        Ok(()) => submit(handle, on_ready).await,
        Err(BlobError::Unavailable(reason)) => Err(reason),
        Err(BlobError::Invalid(reason)) => {
            tracing::debug!(target: "ducktape::saga", reason = "blob_invalid", error = %reason,
                "node work selected its invalid-blob continuation");
            submit(handle, on_invalid).await
        }
    }
}

async fn perform(handle: &NodeHandle, directive: Directive) -> Result<(), String> {
    match directive {
        Directive::Submit(message) => submit(handle, message).await,
        Directive::StageBlob {
            blob,
            on_ready,
            on_invalid,
        } => stage(handle, blob, on_ready, on_invalid).await,
    }
}

async fn advance(handle: &NodeHandle, source: &str) -> Result<(), String> {
    let present = handle
        .status
        .current()
        .modules
        .iter()
        .any(|module| module.id == source);
    if !present {
        return Ok(());
    }
    let Some(signer) = handle.node_signer.as_ref() else {
        return Ok(());
    };
    let key = signer.public_key().as_ref().to_vec();
    let Some(directive) = query(handle, source, key).await? else {
        return Ok(());
    };
    perform(handle, directive).await
}

/// Each source is an updateable module implementing the node-work query.
/// Wakes are hints: always re-query committed state, including after restart.
pub fn spawn(handle: NodeHandle, source: String) {
    let mut blocks = handle.stream_hub().subscribe_blocks();
    tokio::spawn(async move {
        let mut failures = 0u64;
        loop {
            match advance(&handle, &source).await {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let speak = failures.is_power_of_two();
                    if speak {
                        tracing::warn!(target: "ducktape::saga", reason = "node_work_retry",
                            module = %source, attempts = failures, error = %error.lines().next().unwrap_or_default(),
                            "node work will retry at a committed block");
                    }
                }
            }
            match blocks.recv().await {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(
        repo: &git2::Repository,
        data: &[u8],
        mode: i32,
        parent: Option<git2::Oid>,
    ) -> git2::Oid {
        let blob = repo.blob(data).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder.insert("artifact", blob, mode).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.invalid").unwrap();
        let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
        let parents = parent.iter().collect::<Vec<_>>();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "artifact",
            &tree,
            &parents,
        )
        .unwrap()
    }

    fn source(commit: git2::Oid, bytes: &[u8]) -> ForgeBlob {
        ForgeBlob {
            repo: "demo".into(),
            commit: commit.to_string(),
            path: "artifact".into(),
            hash: sha2::Sha256::digest(bytes).into(),
        }
    }

    #[test]
    fn branch_changes_cannot_replace_the_committed_blob() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("demo")).unwrap();
        let original = commit(&repo, b"first blob", 0o100644, None);
        commit(&repo, b"different blob", 0o100644, Some(original));
        let pinned = source(original, b"first blob");
        assert_eq!(read_blob(root.path(), &pinned).unwrap(), b"first blob");
        let wrong = source(original, b"different blob");
        assert!(matches!(
            read_blob(root.path(), &wrong),
            Err(BlobError::Invalid(_))
        ));
    }

    #[test]
    fn staging_refuses_traversal_symlinks_and_abbreviated_revisions() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("demo")).unwrap();
        let original = commit(&repo, b"target", 0o120000, None);
        let symlink = source(original, b"target");
        assert!(matches!(
            read_blob(root.path(), &symlink),
            Err(BlobError::Invalid(_))
        ));
        let mut traversal = symlink.clone();
        traversal.path = "../secret".into();
        assert!(matches!(
            read_blob(root.path(), &traversal),
            Err(BlobError::Invalid(_))
        ));
        let mut bad_repo = symlink.clone();
        bad_repo.repo = "../secret".into();
        assert!(matches!(
            read_blob(root.path(), &bad_repo),
            Err(BlobError::Invalid(_))
        ));
        let mut abbreviated = symlink;
        abbreviated.commit.truncate(7);
        assert!(matches!(
            read_blob(root.path(), &abbreviated),
            Err(BlobError::Invalid(_))
        ));
    }

    #[test]
    fn incomplete_replication_retries_but_an_absent_path_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("demo")).unwrap();
        let mut bytes = b"40000 nested\0".to_vec();
        bytes.extend_from_slice(&[42; 20]);
        let oid = repo
            .odb()
            .unwrap()
            .write(git2::ObjectType::Tree, &bytes)
            .unwrap();
        let tree = repo.find_tree(oid).unwrap();
        assert!(matches!(
            blob_entry(&repo, tree.clone(), "nested/artifact"),
            Err(BlobError::Unavailable(_))
        ));
        assert!(matches!(
            blob_entry(&repo, tree, "absent/artifact"),
            Err(BlobError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn opaque_work_and_both_staging_continuations_use_the_node_key() {
        use futures::StreamExt as _;
        let root = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(root.path().join("demo")).unwrap();
        let oid = commit(&repo, b"any artifact format", 0o100644, None);
        let key = commonware_cryptography::ed25519::PrivateKey::from_seed(19);
        let public = key.public_key().as_ref().to_vec();
        let (handle, mut commands, _) = NodeHandle::channel();
        let blob_root = root.path().join("blobs");
        let handle = handle
            .with_node_signer(key)
            .with_forge_repo(root.path())
            .with_blob_root(&blob_root)
            .unwrap();
        let success = Submission {
            target: "custom-policy".into(),
            payload: b"staged".to_vec(),
        };
        let invalid = Submission {
            target: "another-policy".into(),
            payload: b"refused".to_vec(),
        };
        let blob = source(oid, b"any artifact format");
        // A process can stop after writing a prefix but before its continuation.
        let mut partial = handle
            .blobs
            .stage(blob.hash, b"any artifact format".len() as u64)
            .unwrap();
        partial.append(b"any art").unwrap();
        drop(partial);
        let directives = [
            (Directive::Submit(success.clone()), success.clone()),
            (
                Directive::StageBlob {
                    blob: blob.clone(),
                    on_ready: success.clone(),
                    on_invalid: invalid.clone(),
                },
                success.clone(),
            ),
            // A crash after publication but before the acknowledgement repeats
            // staging. The same opaque continuation remains safe to retry.
            (
                Directive::StageBlob {
                    blob: blob.clone(),
                    on_ready: success.clone(),
                    on_invalid: invalid.clone(),
                },
                success.clone(),
            ),
            (
                Directive::StageBlob {
                    blob: ForgeBlob {
                        hash: [0; 32],
                        ..blob
                    },
                    on_ready: success,
                    on_invalid: invalid.clone(),
                },
                invalid,
            ),
        ];
        for (directive, expected) in directives {
            let read = async {
                let NodeCommand::Submit {
                    target,
                    payload,
                    required_blob: _,
                    origin,
                    reply,
                } = commands.next().await.unwrap()
                else {
                    panic!("opaque node submission")
                };
                assert_eq!(target, expected.target);
                assert_eq!(payload, expected.payload);
                assert_eq!(origin, public);
                reply
                    .send(Ok(crate::BlockSummary {
                        height: 1,
                        root_hash: String::new(),
                    }))
                    .unwrap();
            };
            let (result, ()) = tokio::join!(perform(&handle, directive), read);
            result.unwrap();
        }
        let reopened = blobstore::BlobHandle::persistent(blob_root).unwrap();
        assert!(
            reopened.has_chunk(&source(oid, b"any artifact format").hash),
            "a staging acknowledgement must survive reopening the store"
        );
    }

    #[test]
    fn the_native_dispatch_only_routes_protocol_operations() {
        let file = syn::parse_file(include_str!("node_work.rs")).unwrap();
        let function = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function) if function.sig.ident == "perform" => Some(function),
                _ => None,
            })
            .unwrap();
        assert_eq!(function.block.stmts.len(), 1);
        let syn::Stmt::Expr(syn::Expr::Match(dispatch), None) = &function.block.stmts[0] else {
            panic!("one match")
        };
        for arm in &dispatch.arms {
            assert!(arm.guard.is_none());
            assert!(!matches!(arm.pat, syn::Pat::Wild(_)));
            let syn::Expr::Await(awaited) = &*arm.body else {
                panic!("one async delegation")
            };
            assert!(matches!(*awaited.base, syn::Expr::Call(_)));
        }
        // Keep policy imports out of the bridge when a future workflow grows.
        for item in file.items {
            let syn::Item::Use(import) = item else {
                continue;
            };
            let syn::UseTree::Path(path) = import.tree else {
                continue;
            };
            assert!(
                !["runs", "governance", "modules", "valset"]
                    .iter()
                    .any(|name| path.ident == name)
            );
        }
    }
}
