//! the forge lane: a per-run isolated git clone of a node-local forge repo at the
//! run's pinned commit, committed with agent authorship and pushed back
//! through generic blob storage and node-authorized `PushRefs`, so the branch
//! move settles through consensus with compare-and-swap.
//!
//! host `git` is a REAL runtime dependency of this lane — the first in the
//! tree (forge itself is vendored libgit2 and never shells out). it is probed
//! ONCE at provisioner construction; no worktree-capable `git` ⇒ the lane is
//! permanently unavailable with a loud reason while the duckfs lane keeps
//! working. forge repos are git-default **SHA-1** (see forge/src/git.rs —
//! only pack digests / blob addressing are sha256): never pass
//! `--object-format` anywhere.
//!
//! DETACHED by construction: the clone checks out the PINNED commit with
//! `--detach` and the provisioner never creates or moves a shared-repo ref.
//! this is load-bearing, not style — the consensus module's committed-ref
//! catch-up FORCE-MOVES `refs/heads/<branch>` in the shared repo while runs
//! execute; a checkout HOLDING that branch would silently reparent its
//! commit onto the moved tip with the worktree's old tree (reverting the
//! interloper's content, fast-forward push, no reject ever fired). a
//! detached HEAD is immune: the base stays the pin, and the remote tip is
//! read from the committed module query and fetched by object ID.
//!
//! ordering over CAS-degrade: the push is a plain (never forced)
//! `HEAD:refs/heads/<branch>` update. a concurrently advanced branch rejects
//! at the fork base (production's `PushRefs` CAS, stock git's
//! fetch-first refusal elsewhere), and the reject is an internal RETRY
//! trigger, not a failure: fetch the new tip, rebase the run's commits onto
//! FETCH_HEAD, push again (bounded — [`PUSH_ATTEMPTS`]). the CAS stays the
//! consensus linearizer underneath; both commits land as linear history.
//! ONLY a genuine rebase conflict degrades the receipt (`commit_error` +
//! `Status::Degraded` via the pool), never the reply (R4).

use super::forge_publication::{ModulePublication, Publication};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use compute_service::{
    ProvisionedWorkspace, WorkspaceReceipt, WorkspaceSource, WorkspaceSpec, assemble_context_doc,
};

use crate::node_link::NodeLink;
use provider_host::OperatorCredential;

/// the synthetic domain for agent authorship and attribution. DELIBERATELY in
/// the network's own `.duck` namespace (duckdns), not a registerable TLD: no
/// GitHub account can ever verify these addresses and claim mirrored agent
/// commits. What makes it unownable is that `agents` is a RESERVED root label
/// (`duckdns::RESERVED_ROOT_LABELS`) — consensus rejects it as a handle, so no
/// account can register `agents.duck` and inherit these idents. The address is
/// inert metadata — provenance lives in consensus receipts, never in Git idents.
const AGENT_EMAIL_DOMAIN: &str = "agents.duck";
/// a complete agent-authored commit message.
const MAX_COMMIT_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 128;
/// one node's configured forge lane: where the materialized repos live, where
/// pushes rendezvous, and who the committer is. built by
/// [`ForgeLane::configure`] exactly once, at provisioner construction.
pub(super) struct ForgeLane {
    /// the forge module's on-disk repo base — `<repo_base>/<repo>` is a real
    /// (non-bare, never-checked-out) libgit2 repo the module materializes.
    repo_base: PathBuf,
    publication: Arc<dyn Publication>,
    /// the committer identity on every run commit (the node, never the agent).
    committer_name: String,
}

impl ForgeLane {
    /// Decide the lane once: a repo base and worktree-capable host Git
    /// must exist, otherwise the lane is
    /// `Err(reason)` — permanent for this provisioner's lifetime. the probe
    /// runs only when the materialized repository base is configured.
    pub(super) fn configure(
        node: &NodeLink,
        committer_name: String,
        probe: impl FnOnce() -> Result<(), String>,
    ) -> Result<Self, String> {
        let Some(repo_base) = node.forge_repo().map(Path::to_path_buf) else {
            return Err(
                "this node link carries no forge repo base — the forge module's \
                 materialized repos are not reachable here"
                    .into(),
            );
        };
        probe()?;
        Ok(Self {
            publication: Arc::new(ModulePublication {
                node: node.clone(),
                repo_base: repo_base.clone(),
            }),
            repo_base,
            committer_name,
        })
    }
}

/// the construction-time probe: host `git` exists AND supports the clone and
/// rebase features the runtime invokes. prove those functionally in a
/// scratch repo rather than accepting a version string that can admit a Git
/// binary which only fails after a concurrent push.
pub(super) fn probe_host_git() -> Result<(), String> {
    probe_host_git_with("git")
}

fn probe_host_git_with(program: &str) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let scratch =
        std::env::temp_dir().join(format!("ducktape-git-probe-{}-{nanos}", std::process::id()));
    let result = (|| {
        std::fs::create_dir_all(&scratch)
            .map_err(|e| format!("probe scratch dir {}: {e}", scratch.display()))?;
        run_git_program(program, &scratch, &["init", "-q"], &[])
            .map_err(|e| format!("`git init` failed — is git installed? {e}"))?;
        let identity = [
            ("GIT_AUTHOR_NAME", "Ducktape probe"),
            ("GIT_AUTHOR_EMAIL", "probe@ducktape.local"),
            ("GIT_COMMITTER_NAME", "Ducktape probe"),
            ("GIT_COMMITTER_EMAIL", "probe@ducktape.local"),
        ];
        run_git_program(
            program,
            &scratch,
            &["commit", "--allow-empty", "-q", "-m", "probe"],
            &identity,
        )
        .map_err(|e| format!("`git commit` probe setup failed: {e}"))?;

        let clone = scratch.join("clone");
        let clone_arg = clone.to_string_lossy().into_owned();
        run_git_program(
            program,
            &scratch,
            &["clone", "--local", "--no-hardlinks", "--no-checkout", ".", &clone_arg],
            &[],
        )
        .map_err(|e| {
            format!(
                "`git clone --local --no-hardlinks --no-checkout` failed — host git lacks runtime clone support: {e}"
            )
        })?;
        run_git_program(program, &clone, &["checkout", "--detach", "HEAD"], &[])
            .map_err(|e| format!("`git checkout --detach` failed: {e}"))?;
        run_git_program(
            program,
            &clone,
            &[
                "rebase",
                "--rebase-merges",
                "--reapply-cherry-picks",
                "--empty=keep",
                "HEAD",
            ],
            &identity,
        )
        .map_err(|e| {
            format!(
                "`git rebase --rebase-merges --reapply-cherry-picks --empty=keep` failed — \
                 host git lacks the runtime rebase options: {e}"
            )
        })?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result.map_err(|e: String| {
        format!(
            "host `git` probe failed (forge provisioning needs a runtime-compatible git on PATH): {e}"
        )
    })
}

/// a hermetic git command: no host/global/system config, no prompts, no
/// gpg — identity comes ONLY from the env vars the caller sets (D2). the
/// same posture as the e2e suites' git helpers.
pub(super) fn git(dir: &Path) -> Command {
    git_program("git", dir)
}

fn git_program(program: &str, dir: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_REPLACE_REF_BASE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_TEMPLATE_DIR")
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_PROXY_COMMAND")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ]);
    command
}

/// run a git command to completion; non-zero exit becomes `Err` carrying the
/// command and its stderr (the operator-facing failure text).
pub(super) fn run_git(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    run_git_program("git", dir, args, envs)
}

fn run_git_program(
    program: &str,
    dir: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String, String> {
    let mut command = git_program(program, dir);
    command.args(args);
    for (k, v) in envs {
        command.env(k, v);
    }
    let out = command
        .output()
        .map_err(|e| format!("host `git` failed to spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// the coordinates feed a command line and a path join — refuse anything that
/// could traverse (`..`), read as a git flag (leading `-`), or escape the
/// repo base. consensus already normalized these (norm_repo/norm_branch/hex
/// oids), so a failure here means a corrupt envelope, not a policy gate.
///
/// the branch check carries a SECOND wall (#1836): every run-owned push
/// target lives under `agent/` — a composer that ever named anything else
/// (a PR's own, attacker-chosen source branch included) would be a defect
/// this refusal catches before a single git command runs.
fn validate_coords(repo: &str, commit: &str, branch: &str) -> Result<(), String> {
    let repo_ok = !repo.is_empty()
        && repo != "."
        && repo != ".."
        && !repo.starts_with('-')
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !repo_ok {
        return Err(format!("forge repo name {repo:?} is not a safe repo slug"));
    }
    if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "forge pinned commit {commit:?} is not a 40-hex sha1 oid"
        ));
    }
    let branch_is_safe = !branch.is_empty()
        && !branch.starts_with('-')
        && !branch.starts_with('/')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    if !branch_is_safe {
        return Err(format!(
            "forge work branch {branch:?} is not a safe branch name"
        ));
    }
    let branch_is_agent_scoped = branch.starts_with("agent/") && branch.len() > "agent/".len();
    if !branch_is_agent_scoped {
        return Err(format!(
            "forge work branch {branch:?} is outside the agent/ namespace"
        ));
    }
    Ok(())
}

/// provision one forge run: verify the repo + pinned commit are materialized
/// on THIS node, clone it without hardlinks under
/// the (already D7-validated) run dir, then materialize the W6 skill mounts
/// beside it. `node` is the `/v1` lane those mounts check out over (they are
/// duckfs subtrees whatever the rw source is); `node_url` is this node's http
/// base, handed to the run as `DUCKTAPE_NODE`.
pub(super) async fn provision(
    lane: &ForgeLane,
    node: NodeLink,
    run_dir: PathBuf,
    ro_root: PathBuf,
    node_url: Option<String>,
    session_keys: &Path,
    spec: &WorkspaceSpec,
) -> Result<Box<dyn ProvisionedWorkspace>, String> {
    let WorkspaceSource::Forge {
        repo,
        commit,
        branch,
        ..
    } = &spec.source
    else {
        return Err("forge provisioning invoked on a non-forge workspace spec".into());
    };
    validate_coords(repo, commit, branch)?;
    let repo_dir = lane.repo_base.join(repo);

    let blocking = ProvisionArgs {
        repo_dir,
        run_dir,
        repo: repo.clone(),
        commit: commit.clone(),
    };
    let workspace_args = tokio::task::spawn_blocking(move || provision_blocking(blocking))
        .await
        .map_err(|_| "forge clone provision task panicked".to_string())??;

    // W6 skill ro mounts land at a SUFFIXED SIBLING of the git worktree root
    // (`<slug>-ro/<name>`), never inside it: the commit bracket's `git add -A`
    // stages EVERYTHING under the worktree, so a skill tree in there would be
    // committed with the run's work and PUSHED onto the work branch. beside it,
    // git cannot see them at all.
    let (ro_dir, context_doc) = if spec.ro_mounts.is_empty() {
        // nothing to mount — but the document still ships (see the duckfs lane):
        // the tool-plane instruction and the library pointer are ambient, not
        // curated.
        (None, Some(assemble_context_doc(&[])?))
    } else {
        let mounts = spec.ro_mounts.clone();
        let checkout_ro = ro_root.clone();
        let run_dir = workspace_args.run_dir.clone();
        // the link outlives the mounts: the session bind below rides the same
        // actor lane.
        let mount_node = node.clone();
        // the same step assembles the run's SOUL from the mounts it just
        // materialized — the only place holding both the curation and the bodies.
        let context_doc = tokio::task::spawn_blocking(move || {
            super::checkout_ro_mounts(&mount_node, &checkout_ro, &mounts).inspect_err(|_| {
                // W5: a failed provision removes ALL its own debris. the mount
                // helper dropped its partial ro tree; the clone goes here.
                cleanup_blocking(&run_dir);
            })
        })
        .await
        .map_err(|_| "skill mount checkout task panicked".to_string())??;
        (Some(ro_root), Some(context_doc))
    };

    // the clone EXISTS now, so ask consensus to bind the run's agent session
    // — never before: a bind for a run that failed to materialize would spend an
    // op on a run that never starts.
    let session =
        match super::session::open(&node, spec, &workspace_args.run_dir, session_keys).await {
            Ok(session) => session,
            Err(error) => {
                super::cleanup_dirs(workspace_args.run_dir.clone(), ro_dir.clone()).await;
                return Err(error);
            }
        };
    let mut env = super::run_env(
        &workspace_args.run_dir,
        ro_dir.as_deref(),
        node_url.as_deref(),
        spec,
        session.as_ref(),
    );
    env.insert("GIT_COMMITTER_NAME".into(), lane.committer_name.clone());
    env.insert(
        "GIT_COMMITTER_EMAIL".into(),
        format!("{}@nodes.duck", lane.committer_name),
    );
    Ok(Box::new(ForgeWorkspace {
        run_dir: workspace_args.run_dir,
        ro_dir,
        publication: lane.publication.clone(),
        repo: repo.clone(),
        node,
        source: spec.source.clone(),
        agent: spec.agent.clone(),
        _session: session,
        committer_name: lane.committer_name.clone(),
        env,
        context_doc,
    }))
}

/// the plain-data bundle the blocking provision step consumes and returns.
struct ProvisionArgs {
    repo_dir: PathBuf,
    run_dir: PathBuf,
    repo: String,
    commit: String,
}

fn provision_blocking(args: ProvisionArgs) -> Result<ProvisionArgs, String> {
    let ProvisionArgs {
        repo_dir,
        run_dir,
        repo,
        commit,
    } = &args;
    if !repo_dir.join(".git").exists() {
        return Err(format!(
            "forge repo {repo:?} is not materialized on this node ({} is not a git repo)",
            repo_dir.display()
        ));
    }
    // the pinned commit must be ON DISK: committed refs can lead the local
    // pack (materialization lag — a missing/late pack blob leaves the on-disk
    // repo behind the committed head). a wrong-base worktree must never be
    // minted; the attempt fails and the saga's re-lease owns liveness.
    let present = git(repo_dir)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .output()
        .map_err(|e| format!("host `git` failed to spawn: {e}"))?;
    if !present.status.success() {
        return Err(format!(
            "pinned commit {commit} is not present in forge repo {repo:?} on this node \
             (pack materialization can lag committed refs): {}",
            String::from_utf8_lossy(&present.stderr).trim()
        ));
    }
    // A SELF-CONTAINED clone keeps `.git` inside the workdir, which is the only
    // thing that crosses into the guest (as its workspace device — a `.git`
    // outside it would simply not be there).
    // `--no-hardlinks` is load-bearing: the untrusted run must not be
    // able to corrupt the node's canonical object store through a shared inode.
    let cloned = git(repo_dir)
        .args(["clone", "--local", "--no-hardlinks", "--no-checkout"])
        .arg(repo_dir.as_os_str())
        .arg(run_dir.as_os_str())
        .output()
        .map_err(|e| format!("host `git` failed to spawn: {e}"))?;
    if !cloned.status.success() {
        cleanup_blocking(run_dir);
        return Err(format!(
            "git clone for forge repo {repo:?} failed: {}",
            String::from_utf8_lossy(&cloned.stderr).trim()
        ));
    }
    if let Err(e) = run_git(run_dir, &["checkout", "--detach", commit], &[]) {
        cleanup_blocking(run_dir);
        return Err(format!(
            "git detached checkout for forge repo {repo:?} at {commit} failed: {e}"
        ));
    }
    if let Err(e) = run_git(run_dir, &["remote", "remove", "origin"], &[]) {
        cleanup_blocking(run_dir);
        return Err(format!(
            "removing the canonical origin from forge repo {repo:?} failed: {e}"
        ));
    }
    Ok(args)
}

/// one live forge clone: the source repo, its own checkout
/// dir, and everything the commit-and-push needs.
struct ForgeWorkspace {
    run_dir: PathBuf,
    /// the W6 skill ro root (`<slug>-ro`), `Some` iff the run had mounts —
    /// tracked ONLY so cleanup can remove it; the commit/push never look at it
    /// (it lives outside the worktree, so git cannot see it either).
    ro_dir: Option<PathBuf>,
    publication: Arc<dyn Publication>,
    repo: String,
    /// Credentials are read from the live link for each publication.
    node: NodeLink,
    source: WorkspaceSource,
    agent: Option<compute_service::AgentExecution>,
    committer_name: String,
    env: BTreeMap<String, String>,
    /// the run's assembled soul — its `always` skills inlined, the rest indexed.
    /// `None` when the agent curated no skills. the provider delivers it.
    context_doc: Option<String>,
    /// Owns the scoped signer endpoint for exactly as long as the workspace.
    _session: Option<super::session::RunSession>,
}

impl ForgeWorkspace {
    /// a receipt-only spec, same trick as the duckfs lane: the constructors
    /// read only the source coords.
    fn receipt_spec(&self) -> WorkspaceSpec {
        WorkspaceSpec {
            run_id: String::new(),
            agent: None,
            source: self.source.clone(),
            ro_mounts: Vec::new(),
        }
    }

    /// the source's pinned base commit / work branch (forge variant by
    /// construction — provision refused anything else).
    fn coords(&self) -> (String, String, String) {
        match &self.source {
            WorkspaceSource::Forge {
                commit,
                branch,
                item_title,
                ..
            } => (commit.clone(), branch.clone(), item_title.clone()),
            WorkspaceSource::Duckfs { .. } => (String::new(), String::new(), String::new()),
        }
    }
}

/// what the blocking commit step concluded.
enum CommitOutcome {
    /// clean tree AND the branch still sits at the pinned base — nothing to
    /// push, a true `no_changes`.
    NoChanges,
    /// the work landed: the branch was pushed and this is its new head oid.
    /// `rebased` = a concurrent advance was folded under it first (rides the
    /// receipt's EXISTING `rebased` field — the duckfs semantic twin).
    Pushed { oid: String, rebased: bool },
}

/// total push attempts before a rejection is a real failure. a concurrent
/// branch advance is an ORDERING problem, not a degrade: the consensus CAS
/// underneath stays the linearizer, a reject just triggers fetch → rebase →
/// push like a git user would. bounded so a hot/hostile remote can't spin
/// the commit bracket.
const PUSH_ATTEMPTS: u32 = 3;

/// Use the committed Forge item title for the primary capture commit. Model
/// prose is publication-body material, not identity metadata; it is only a
/// fallback when the item title is unavailable (empty) or structurally unsafe.
fn select_commit_message(
    response_proposal: Option<&str>,
    item_title: &str,
) -> Result<String, String> {
    std::iter::once(item_title)
        .chain(response_proposal)
        .find_map(commit_message_candidate)
        .ok_or_else(|| {
            "Forge item title and response fallback were both missing or invalid".to_string()
        })
}

fn commit_message_candidate(candidate: &str) -> Option<String> {
    // Keep the message byte-for-byte. The only rejected bytes are controls Git
    // history should never carry; CR/LF and tabs are legitimate prose bytes.
    let invalid = candidate
        .chars()
        .any(|c| !matches!(c, '\r' | '\n' | '\t') && c.is_control());
    if invalid
        || candidate.trim().is_empty()
        || candidate.len() > MAX_COMMIT_MESSAGE_BYTES
        || candidate.lines().any(is_identity_trailer)
    {
        return None;
    }
    Some(candidate.to_owned())
}

fn is_identity_trailer(line: &str) -> bool {
    let Some((key, _)) = line.trim().split_once(':') else {
        return false;
    };
    let key = key.trim().to_ascii_lowercase();
    key.ends_with("-by")
        || matches!(
            key.as_str(),
            "author" | "committer" | "cc" | "from" | "on-behalf-of"
        )
}

fn remove_git_control_file(git_dir: &Path, relative: &str) -> Result<(), String> {
    let path = git_dir.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            Err(format!("agent replaced .git/{relative} with a directory"))
        }
        Ok(_) => std::fs::remove_file(&path)
            .map_err(|e| format!("failed to remove agent-controlled .git/{relative}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to inspect .git/{relative}: {e}")),
    }
}

fn reject_git_symlinks(git_dir: &Path) -> Result<(), String> {
    let mut pending = vec![git_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("failed to inspect agent Git metadata: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("failed to inspect agent Git metadata: {e}"))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| format!("failed to inspect agent Git metadata: {e}"))?;
            if kind.is_symlink() {
                let relative = path
                    .strip_prefix(git_dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                return Err(format!("agent Git metadata contains symlink {relative:?}"));
            }
            if kind.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

/// The checkout's `.git` directory is writable inside the sandbox so the
/// agent can commit. Before host Git touches it, remove every local execution
/// or history-virtualization control the agent could have installed. Commit
/// objects, refs, index, and messages remain untouched.
fn sanitize_agent_git_control(run_dir: &Path) -> Result<(), String> {
    let git_dir = run_dir.join(".git");
    let meta = std::fs::symlink_metadata(&git_dir)
        .map_err(|e| format!("failed to inspect agent .git directory: {e}"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err("agent workspace .git is not a real directory".into());
    }
    reject_git_symlinks(&git_dir)?;
    for path in [
        "config",
        "config.worktree",
        "commondir",
        "info/grafts",
        "objects/info/alternates",
        "objects/info/http-alternates",
        "shallow",
    ] {
        remove_git_control_file(&git_dir, path)?;
    }
    std::fs::write(git_dir.join("config"), b"")
        .map_err(|e| format!("failed to install a clean local Git config: {e}"))
}

pub(super) fn sanitize_display_name(input: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in input.chars() {
        if c.is_control() || matches!(c, '<' | '>') {
            continue;
        }
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        let add_space = if pending_space { 1 } else { 0 };
        if out.len() + add_space + c.len_utf8() > MAX_DISPLAY_NAME_BYTES {
            break;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    if out.is_empty() {
        "Ducktape Agent".into()
    } else {
        out
    }
}

/// The agent's address. Consensus admits only DNS-label agent ids
/// (`crate::runs::validate_agent_id`), and a label fits an RFC 5321 local part
/// verbatim — so `quackbot` attributes to `quackbot@agents.duck` and the
/// address round-trips back to the registry key.
fn attribution_email_local_part(input: &str) -> String {
    debug_assert!(crate::runs::validate_agent_id(input).is_ok());
    input.to_owned()
}

/// the address a run's commits are authored under: the agent's id at
/// [`AGENT_EMAIL_DOMAIN`].
pub(super) fn agent_email(agent_id: &str) -> String {
    format!(
        "{}@{AGENT_EMAIL_DOMAIN}",
        attribution_email_local_part(agent_id)
    )
}

fn commit_message(run_dir: &Path, oid: &str) -> Result<String, String> {
    let out = git(run_dir)
        .args(["show", "-s", "--format=%B", oid])
        .output()
        .map_err(|e| format!("host `git` failed to spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git show of agent commit {oid} message failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout)
        .map_err(|_| format!("agent commit {oid} message is not valid UTF-8"))
}

/// what the agent's own commits are worth once read.
enum AgentHistory {
    /// every commit is the run's author and the node's committer with a clean
    /// message: pushed as the agent wrote it.
    Kept,
    /// this commit names another identity or carries an unsafe message; the
    /// reason is a stable snake_case token.
    Refused {
        commit: String,
        reason: &'static str,
    },
}

fn validate_agent_commits(
    run_dir: &Path,
    pinned_commit: &str,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
) -> Result<AgentHistory, String> {
    if run_git(
        run_dir,
        &["merge-base", "--is-ancestor", pinned_commit, "HEAD"],
        &[],
    )
    .is_err()
    {
        return Err(
            "agent-created Git history does not descend from the pinned Forge commit".into(),
        );
    }
    let range = format!("{pinned_commit}..HEAD");
    let commits = run_git(run_dir, &["rev-list", "--reverse", &range], &[])?;
    let expected_identity =
        format!("{author_name}\0{author_email}\0{committer_name}\0{committer_email}");
    for oid in commits.lines() {
        let identity = run_git(
            run_dir,
            &["show", "-s", "--format=%an%x00%ae%x00%cn%x00%ce", oid],
            &[],
        )?;
        if identity != expected_identity {
            return Ok(AgentHistory::Refused {
                commit: oid.to_string(),
                reason: "foreign_identity",
            });
        }
        let message = commit_message(run_dir, oid)?;
        if commit_message_candidate(&message).is_none() {
            return Ok(AgentHistory::Refused {
                commit: oid.to_string(),
                reason: "unsafe_message",
            });
        }
    }
    Ok(AgentHistory::Kept)
}

/// the commit the run's output is built on. the agent's own commits are kept
/// when every one is theirs and clean. one that names another identity or
/// carries an unsafe message costs the history, never the work: the branch
/// drops back to the pinned commit with the agent's tree still staged, and the
/// node captures that tree in its own commit. history that does not descend
/// from the pin is refused outright.
fn agent_history_base(
    run_dir: &Path,
    pinned_commit: &str,
    head: &str,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
    committer_email: &str,
) -> Result<String, String> {
    if head == pinned_commit {
        return Ok(head.to_string());
    }
    let history = validate_agent_commits(
        run_dir,
        pinned_commit,
        author_name,
        author_email,
        committer_name,
        committer_email,
    )?;
    let AgentHistory::Refused { commit, reason } = history else {
        return Ok(head.to_string());
    };
    tracing::warn!(
        target: "ducktape::agent", event = "agent_history_dropped",
        commit = commit.as_str(), reason,
        "agent commits dropped; their tree is captured in the node's commit"
    );
    run_git(run_dir, &["reset", "--soft", pinned_commit], &[])?;
    Ok(pinned_commit.to_string())
}

fn create_run_commit(
    run_dir: &Path,
    tree: &str,
    parent_commit: &str,
    message: &str,
    author_name: &str,
    author_email: &str,
    committer_name: &str,
) -> Result<String, String> {
    let committer_email = format!("{committer_name}@nodes.duck");
    let mut command = git(run_dir);
    command
        .args(["commit-tree", tree, "-p", parent_commit])
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", committer_name)
        .env("GIT_COMMITTER_EMAIL", committer_email)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("host `git commit-tree` failed to spawn: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "git commit-tree stdin was unavailable".to_string())?
        .write_all(message.as_bytes())
        .map_err(|e| format!("git commit-tree message write failed: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("git commit-tree wait failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git commit-tree failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout)
        .map(|oid| oid.trim().to_string())
        .map_err(|_| "git commit-tree returned a non-utf8 oid".to_string())
}

struct CommitIdentity {
    agent_display_name: String,
    agent_id: String,
    committer_name: String,
}

#[allow(clippy::too_many_arguments)]
fn commit_blocking(
    run_dir: &Path,
    pinned_commit: &str,
    branch: &str,
    publication: &dyn Publication,
    repo: &str,
    response_proposal: Option<&str>,
    item_title: &str,
    identity: &CommitIdentity,
) -> Result<CommitOutcome, String> {
    // The provider's isolated HOME/auth/temp/target tree lives inside the
    // disk-backed run worktree but is runtime debris, not authored source.
    // Delete it before `git add -A` so credentials and caches cannot be pushed.
    let _ = std::fs::remove_dir_all(run_dir.join(provider_host::RUN_RUNTIME_DIR));
    sanitize_agent_git_control(run_dir)?;
    let head = run_git(run_dir, &["rev-parse", "HEAD"], &[])?;
    let safe_display_name = sanitize_display_name(&identity.agent_display_name);
    let author_email = format!(
        "{}@{AGENT_EMAIL_DOMAIN}",
        attribution_email_local_part(&identity.agent_id)
    );
    let committer_email = format!("{}@nodes.duck", identity.committer_name);
    let base = agent_history_base(
        run_dir,
        pinned_commit,
        &head,
        &safe_display_name,
        &author_email,
        &identity.committer_name,
        &committer_email,
    )?;

    run_git(run_dir, &["add", "-A"], &[])?;
    let final_tree = run_git(run_dir, &["write-tree"], &[])?;
    let head_tree = run_git(run_dir, &["rev-parse", "HEAD^{tree}"], &[])?;
    if base == pinned_commit && final_tree == head_tree {
        return Ok(CommitOutcome::NoChanges);
    }
    // the run produced something to push — an agent-authored commit, a
    // working-tree change, or both.
    if final_tree != head_tree {
        let message = select_commit_message(response_proposal, item_title)?;
        let oid = create_run_commit(
            run_dir,
            &final_tree,
            &base,
            &message,
            &safe_display_name,
            &author_email,
            &identity.committer_name,
        )?;
        run_git(run_dir, &["reset", "--hard", &oid], &[])?;
    }
    // plain push, NEVER --force. a rejection means the branch advanced while
    // the run executed — an ordering problem, so do what a git user does:
    // fetch the new tip, rebase the run's commits onto it (the author
    // survives a rebase natively; the committer stays the node via the same
    // env), push again. ONLY a genuine rebase conflict degrades (Err → the
    // pool's commit_failed + Degraded; the reply still delivers, R4), and
    // the interloper's tip stays branch head.
    let committer_env = [
        ("GIT_COMMITTER_NAME", identity.committer_name.as_str()),
        ("GIT_COMMITTER_EMAIL", committer_email.as_str()),
    ];
    let mut rebased = false;
    for attempt in 1..=PUSH_ATTEMPTS {
        match publication.push(repo, run_dir, branch) {
            Ok(_) => {
                // re-read AFTER any rebase: the pushed head is the output_commit.
                let oid = run_git(run_dir, &["rev-parse", "HEAD"], &[])?;
                return Ok(CommitOutcome::Pushed { oid, rebased });
            }
            Err(e) if attempt == PUSH_ATTEMPTS => {
                return Err(format!(
                    "push of branch {branch:?} in {repo} was rejected after \
                     {PUSH_ATTEMPTS} attempts: {e}"
                ));
            }
            Err(_) => {
                // the remote tip comes ONLY from fetch — never the local
                // shared ref, which consensus catch-up moves/leaves stale. a
                // fetch miss means the branch is unborn remotely (a create
                // race that resolved away, or a non-tip reject like a hook):
                // skip the rebase and just push again.
                if publication.fetch(repo, run_dir, branch).is_ok() {
                    run_git(
                        run_dir,
                        &[
                            "rebase",
                            "--rebase-merges",
                            "--reapply-cherry-picks",
                            "--empty=keep",
                            "FETCH_HEAD",
                        ],
                        &committer_env,
                    )
                    .map_err(|e| {
                        // never leave the worktree mid-rebase; the
                        // interloper's tip stays branch head (best-effort
                        // abort, then degrade).
                        let _ = git(run_dir).args(["rebase", "--abort"]).output();
                        format!("rebase conflict on {branch:?}: {e}")
                    })?;
                    rebased = true;
                }
            }
        }
    }
    unreachable!("the push loop returns on every arm of its final attempt")
}

/// remove the run clone — shared by the W5 cleanup and provision's own error
/// path (self-cleanup of debris).
/// idempotent, best-effort: every error is swallowed (cleanup must never
/// fail the run, and an already-gone dir is success).
fn cleanup_blocking(run_dir: &Path) {
    let _ = std::fs::remove_dir_all(run_dir);
}

#[async_trait::async_trait]
impl ProvisionedWorkspace for ForgeWorkspace {
    fn workdir(&self) -> PathBuf {
        self.run_dir.clone()
    }

    fn env(&self) -> BTreeMap<String, String> {
        self.env.clone()
    }

    fn context_doc(&self) -> Option<String> {
        self.context_doc.clone()
    }

    fn native_conversation(&self) -> Option<provider_host::NativeConversationContext> {
        self._session.as_ref()?.native_conversation.clone()
    }

    fn operator_credential(&self) -> Option<OperatorCredential> {
        Some(super::operator_credential(&self.node))
    }

    /// the pinned repo, and nothing wider: this run's lane lends the operator
    /// credential to a push on THIS repo only, whatever repo the guest names.
    fn forge_repo(&self) -> Option<String> {
        Some(self.repo.clone())
    }

    async fn commit(
        &self,
        _audit_message: &str,
        proposal: Option<&str>,
    ) -> Result<WorkspaceReceipt, String> {
        let (pinned_commit, branch, item_title) = self.coords();
        let run_dir = self.run_dir.clone();
        let publication = self.publication.clone();
        let repo = self.repo.clone();
        let (agent_id, agent_display_name) = match &self.agent {
            Some(agent) => (agent.agent_id.clone(), agent.display_name.clone()),
            None => ("agent".into(), "agent".into()),
        };
        let committer_name = self.committer_name.clone();
        let proposal = proposal.map(str::to_owned);
        let outcome = tokio::task::spawn_blocking(move || {
            let identity = CommitIdentity {
                agent_display_name,
                agent_id,
                committer_name,
            };
            commit_blocking(
                &run_dir,
                &pinned_commit,
                &branch,
                publication.as_ref(),
                &repo,
                proposal.as_deref(),
                &item_title,
                &identity,
            )
        })
        .await
        .map_err(|_| "forge workspace commit task panicked".to_string())??;
        let spec = self.receipt_spec();
        Ok(match outcome {
            CommitOutcome::NoChanges => WorkspaceReceipt::no_changes(&spec),
            CommitOutcome::Pushed { oid, rebased } => {
                let mut receipt = WorkspaceReceipt::pushed(&spec, oid);
                // the receipt's EXISTING field, the duckfs semantic twin
                // (work landed on a moved base) — no wire change.
                receipt.rebased = rebased;
                receipt
            }
        })
    }

    async fn cleanup(&self) {
        if let Some(session) = &self._session {
            session.release();
        }
        super::cleanup_dirs(self.run_dir.clone(), self.ro_dir.clone()).await;
    }
}

#[cfg(test)]
#[path = "forge_tests.rs"]
mod tests;
