//! Ref publication effects are separate from commit/rebase decisions. Production
//! publishes the node's own module operation; tests can supply a Git rendezvous.
use super::forge::run_git;
use crate::node_link::NodeLink;
use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
};

pub(super) trait Publication: Send + Sync {
    fn push(&self, repo: &str, run: &Path, branch: &str) -> Result<(), String>;
    fn fetch(&self, repo: &str, run: &Path, branch: &str) -> Result<(), String>;
}

pub(super) struct ModulePublication {
    pub node: NodeLink,
    pub repo_base: PathBuf,
}
impl ModulePublication {
    fn refs(&self, repo: &str) -> Result<std::collections::BTreeMap<String, String>, String> {
        let query = forge::encode_query(&forge::ForgeQuery::ListRefs { repo: repo.into() });
        let answer =
            tokio::runtime::Handle::current().block_on(self.node.query("forge", &query))?;
        let forge::ForgeReply::Refs(refs) =
            forge::decode_reply(&answer).map_err(|error| error.to_string())?
        else {
            return Err("unexpected Forge refs reply".into());
        };
        Ok(refs
            .into_iter()
            .map(|entry| (entry.name, entry.head))
            .collect())
    }
}
impl Publication for ModulePublication {
    fn push(&self, repo: &str, run: &Path, branch: &str) -> Result<(), String> {
        let head = run_git(run, &["rev-parse", "HEAD"], &[])?;
        let refs = self.refs(repo)?;
        let previous = refs.get(branch).map(String::as_str);
        if let Some(previous) = previous {
            run_git(run, &["merge-base", "--is-ancestor", previous, &head], &[])
                .map_err(|_| "committed branch advanced; fetch and rebase required".to_owned())?;
        }
        let pack = pack(run, &head, refs.values().map(String::as_str))?;
        let runtime = tokio::runtime::Handle::current();
        let digest = runtime.block_on(self.node.put_blob(pack))?;
        let required_blob: [u8; 32] = digest
            .as_slice()
            .try_into()
            .map_err(|_| "invalid blob digest length")?;
        let oid = |hex: &str| {
            git2::Oid::from_str(hex)
                .map(|oid| oid.as_bytes().to_vec())
                .map_err(|error| error.to_string())
        };
        let message = forge::ForgeMsg::PushRefs {
            repo: repo.into(),
            updates: vec![forge::RefUpdate {
                ref_name: branch.into(),
                prev_oid: previous.map(oid).transpose()?,
                new_oid: Some(oid(&head)?),
            }],
            pack_digest: Some(digest),
            cert: None,
        };
        runtime.block_on(self.node.submit_with_blob(
            "forge",
            &forge::encode_msg(&message),
            Some(required_blob),
        ))?;
        Ok(())
    }
    fn fetch(&self, repo: &str, run: &Path, branch: &str) -> Result<(), String> {
        let head = self.refs(repo)?.remove(branch).ok_or("branch is unborn")?;
        let source = self.repo_base.join(repo);
        run_git(
            run,
            &[
                "fetch",
                source.to_str().ok_or("Git store path is not UTF-8")?,
                &head,
            ],
            &[],
        )?;
        Ok(())
    }
}

fn pack<'a>(
    run: &Path,
    head: &str,
    known: impl Iterator<Item = &'a str>,
) -> Result<Vec<u8>, String> {
    let repository = git2::Repository::open(run).map_err(|error| error.to_string())?;
    let mut exclusions = Vec::new();
    for hash in known {
        let oid = git2::Oid::from_str(hash).map_err(|error| error.to_string())?;
        let available = repository.find_commit(oid).is_ok();
        if available {
            exclusions.push(oid);
        }
    }
    // A new branch can share the entire existing history. Exclude committed
    // objects already present at the destination, just as Git negotiation does.
    let mut child = super::forge::git(run)
        .args(["pack-objects", "--stdout", "--revs", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("pack process: {error}"))?;
    let result: Result<Vec<u8>, String> = (|| {
        let mut input = child.stdin.take().ok_or("pack stdin unavailable")?;
        writeln!(input, "{head}").map_err(|error| error.to_string())?;
        for previous in exclusions {
            writeln!(input, "^{previous}").map_err(|error| error.to_string())?;
        }
        drop(input);
        // no ceiling on the pack: an agent's branch may be its first, carrying
        // a whole history, and the blob lane it rides streams.
        let mut output = child.stdout.take().ok_or("pack stdout unavailable")?;
        let mut bytes = Vec::new();
        output
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    let bytes = result?;
    if !status.success() {
        return Err("Git pack creation failed".into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(repo: &git2::Repository, parent: Option<git2::Oid>, text: &[u8]) -> git2::Oid {
        let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
        let base = parent.as_ref().map(|commit| commit.tree().unwrap());
        let mut tree = repo.treebuilder(base.as_ref()).unwrap();
        tree.insert("answer", repo.blob(text).unwrap(), 0o100644)
            .unwrap();
        let tree = repo.find_tree(tree.write().unwrap()).unwrap();
        let signature =
            git2::Signature::new("Test", "test@example.invalid", &git2::Time::new(1, 0)).unwrap();
        repo.commit(
            None,
            &signature,
            &signature,
            "work",
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn import(repo: &git2::Repository, bytes: &[u8]) {
        let odb = repo.odb().unwrap();
        let mut writer = odb.packwriter().unwrap();
        writer.write_all(bytes).unwrap();
        writer.commit().unwrap();
    }

    #[test]
    fn new_branch_transfers_only_work_missing_from_committed_refs() {
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(source.path()).unwrap();
        let receiver = git2::Repository::init(destination.path()).unwrap();
        let base = commit(&repo, None, b"existing history");
        let next = commit(&repo, Some(base), b"new work");
        let known = [base.to_string(), "f".repeat(40)];
        let delta = pack(
            source.path(),
            &next.to_string(),
            known.iter().map(String::as_str),
        )
        .unwrap();
        import(&receiver, &delta);
        assert!(
            receiver.find_commit(base).is_err(),
            "existing history must not be resent"
        );
        import(
            &receiver,
            &pack(source.path(), &base.to_string(), std::iter::empty()).unwrap(),
        );
        let restored = receiver.find_commit(next).unwrap();
        assert_eq!(restored.parent_id(0).unwrap(), base);
        let tree = restored.tree().unwrap();
        let entry = tree.get_name("answer").unwrap();
        assert_eq!(
            receiver.find_blob(entry.id()).unwrap().content(),
            b"new work"
        );
    }
}
