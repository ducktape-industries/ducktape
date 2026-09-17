//! forge-lane tests against temp SHA-1 repos constructed the way forge
//! materializes them: libgit2 init (non-bare, initial_head main, hermetic),
//! commits written straight into the odb with the ref set and NOTHING ever
//! checked out. the provisioner ops run REAL host `git` (same as production);
//! test publication uses a `file://` bare repo. Stock Git's fetch-first
//! refusal is the compare-and-swap rejection stand-in; production uses
//! generic blob upload and a committed Forge module operation.

use super::super::NodedProvisioner;
use super::super::plane_tests::{
    SKILL_BODY, SKILL_FILE, skill_mount, skill_tree, spawn_files_actor,
};
use super::*;
use crate::NodeHandle;
use compute_service::WorkspaceProvisioner as _;
use runs_wire as runs;

/// write `body` to `path` as an executable script, through a child shell
/// rather than this process: a file this process holds open for writing is
/// inherited by every child another test forks meanwhile, and executing it
/// while such a child still holds the descriptor fails with ETXTBSY.
fn install_script(path: &std::path::Path, body: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("umask 022 && cat > \"$0\" && chmod 755 \"$0\"")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(body.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success(), "install {}", path.display());
}

const REPO: &str = "app";
const BRANCH: &str = "agent/item-7";
const AGENT: &str = "quackbot";
const AGENT_DISPLAY_NAME: &str = "Quack Agent";
const NODE_IDENT: &str = "node-f00f";

/// init a repo EXACTLY as forge::git::init does (that fn is crate-private):
/// non-bare, `initial_head("main")`, `external_template(false)` — sha1 by
/// default, `.git` exists, working tree never checked out.
fn init_repo(dir: &Path) -> git2::Repository {
    std::fs::create_dir_all(dir).unwrap();
    let mut opts = git2::RepositoryInitOptions::new();
    opts.initial_head("main").external_template(false);
    git2::Repository::init_opts(dir, &opts).unwrap()
}

/// write a commit straight into the odb (no index, no checkout — forge's
/// materialization shape) and return its hex oid. moves NO ref.
fn odb_commit(repo: &git2::Repository, parent: Option<&str>, path: &str, content: &str) -> String {
    let blob = repo.blob(content.as_bytes()).unwrap();
    let parent_commit =
        parent.map(|hex| repo.find_commit(git2::Oid::from_str(hex).unwrap()).unwrap());
    let base_tree = parent_commit.as_ref().map(|c| c.tree().unwrap());
    let mut tb = repo.treebuilder(base_tree.as_ref()).unwrap();
    tb.insert(path, blob, 0o100644).unwrap();
    let tree = repo.find_tree(tb.write().unwrap()).unwrap();
    let t = git2::Time::new(1_000, 0);
    let sig = git2::Signature::new("ducktape", "ducktape@localhost", &t).unwrap();
    let parents: Vec<&git2::Commit> = parent_commit.iter().collect();
    repo.commit(None, &sig, &sig, "seed", &tree, &parents)
        .unwrap()
        .to_string()
}

fn set_ref(repo: &git2::Repository, branch: &str, oid: &str) {
    repo.reference(
        &format!("refs/heads/{branch}"),
        git2::Oid::from_str(oid).unwrap(),
        true,
        "test",
    )
    .unwrap();
}

fn ref_oid(repo_dir: &Path, branch: &str) -> Option<String> {
    let repo = git2::Repository::open(repo_dir).unwrap();
    repo.refname_to_id(&format!("refs/heads/{branch}"))
        .ok()
        .map(|o| o.to_string())
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    run_git(dir, args, &[]).unwrap_or_else(|e| panic!("git {args:?}: {e}"))
}

/// `unwrap_err` for provision results (`dyn ProvisionedWorkspace` is not Debug).
#[allow(clippy::type_complexity)]
fn provision_err(result: Result<Box<dyn ProvisionedWorkspace>, String>) -> String {
    match result {
        Ok(_) => panic!("provision unexpectedly succeeded"),
        Err(e) => e,
    }
}

/// registered (non-prunable) worktrees of `repo_dir`, primary included.
fn worktree_count(repo_dir: &Path) -> usize {
    git_stdout(repo_dir, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

/// one test bed: a materialized-shape substrate repo at `<base>/app` with
/// `main` born at `head`, a bare-repo rendezvous dir for pushes, and a runs
/// root — all under one temp dir.
struct Bed {
    _tmp: tempfile::TempDir,
    repo_base: PathBuf,
    repo_dir: PathBuf,
    bares: PathBuf,
    runs_root: PathBuf,
    head: String,
}

fn bed() -> Bed {
    let tmp = tempfile::tempdir().unwrap();
    let repo_base = tmp.path().join("forge-git");
    let repo_dir = repo_base.join(REPO);
    let repo = init_repo(&repo_dir);
    let head = odb_commit(&repo, None, "readme.md", "hello\n");
    set_ref(&repo, "main", &head);
    let bares = tmp.path().join("bares");
    std::fs::create_dir_all(&bares).unwrap();
    let runs_root = tmp.path().join("runs");
    std::fs::create_dir_all(&runs_root).unwrap();
    Bed {
        _tmp: tmp,
        repo_base,
        repo_dir,
        bares,
        runs_root,
        head,
    }
}

impl Bed {
    fn push_base(&self) -> String {
        format!("file://{}", self.bares.display())
    }

    async fn provisioner(&self) -> NodedProvisioner {
        let (handle, rx, _hub) = NodeHandle::channel();
        spawn_files_actor(rx, Default::default(), false);
        NodedProvisioner::new(
            crate::agent_provision::test_link(handle.with_forge_repo(&self.repo_base)).await,
            &self.runs_root,
        )
        .with_forge(NODE_IDENT)
        .with_test_publication(self.push_base())
    }

    /// A served actor with skill files. `reject_reads` fails their checkout
    /// midway; session binds use the same actor lane as the plain provisioner.
    async fn skill_provisioner(
        &self,
        reject_reads: bool,
    ) -> (NodedProvisioner, tokio::task::JoinHandle<()>) {
        let (handle, rx, _hub) = NodeHandle::channel();
        let actor = spawn_files_actor(rx, skill_tree(), reject_reads);
        let prov = NodedProvisioner::new(
            crate::agent_provision::test_link(handle.with_forge_repo(&self.repo_base)).await,
            &self.runs_root,
        )
        .with_forge(NODE_IDENT)
        .with_test_publication(self.push_base());
        (prov, actor)
    }

    /// [`Self::spec`] with the W6 skill mount the plane tests share.
    fn skill_spec(&self, run_id: &str, commit: &str) -> WorkspaceSpec {
        WorkspaceSpec {
            ro_mounts: vec![skill_mount()],
            ..self.spec(run_id, commit, false)
        }
    }

    /// snapshot the substrate into the bare rendezvous repo (ALL refs) —
    /// the stand-in for the forge remote's committed state at compose time.
    fn snapshot_bare(&self) -> PathBuf {
        let dst = self.bares.join(REPO);
        run_git(
            &self.bares,
            &[
                "clone",
                "--bare",
                "-q",
                self.repo_dir.to_str().unwrap(),
                dst.to_str().unwrap(),
            ],
            &[],
        )
        .unwrap();
        dst
    }

    fn spec(&self, run_id: &str, commit: &str, branch_born: bool) -> WorkspaceSpec {
        WorkspaceSpec {
            run_id: run_id.into(),
            // the forge lane's session bind rides the same id as every other:
            // the one `runs` resolves (a forge item run is a chat run on the
            // item's channel), never the host-local `run_id` above.
            agent: Some(compute_service::AgentExecution {
                native_conversation: None,
                run_id: runs::run_id_for(&format!("forge:{REPO}:7"), 1, AGENT),
                attempt: 0,
                agent_id: AGENT.into(),
                display_name: AGENT_DISPLAY_NAME.into(),
            }),
            source: WorkspaceSource::Forge {
                repo: REPO.into(),
                item_title: "Fix the flaky gate".into(),
                commit: commit.into(),
                branch: BRANCH.into(),
                branch_born,
            },
            ro_mounts: Vec::new(),
        }
    }
}

// ---- construction: probe + config legs -----------------------------------

#[test]
fn the_probe_passes_on_a_runtime_compatible_host_git() {
    // the suite itself requires host git; the REAL probe must agree.
    probe_host_git().expect("this box has a runtime-compatible git");
}

#[test]
fn the_probe_fails_loud_when_git_is_absent() {
    let err = probe_host_git_with("ducktape-no-such-git-binary").unwrap_err();
    assert!(
        err.contains("probe failed") && err.contains("failed to spawn"),
        "the failure names the probe and the cause: {err}"
    );
}

#[cfg(unix)]
#[test]
fn the_probe_rejects_git_without_the_runtime_rebase_options() {
    let tmp = tempfile::tempdir().unwrap();
    let shim = tmp.path().join("git-with-old-rebase");
    install_script(
        &shim,
        "#!/bin/sh\n\
         for arg in \"$@\"; do\n\
           case \"$arg\" in\n\
             --rebase-merges|--reapply-cherry-picks|--empty=keep)\n\
               echo \"error: unknown option $arg\" >&2\n\
               exit 129\n\
               ;;\n\
           esac\n\
         done\n\
         exec git \"$@\"\n",
    );

    let err = probe_host_git_with(shim.to_str().unwrap()).unwrap_err();
    assert!(err.contains("runtime-compatible git"), "{err}");
    assert!(err.contains("runtime rebase options"), "{err}");
    assert!(err.contains("unknown option"), "{err}");
}

#[tokio::test]
async fn a_failed_probe_is_permanent_and_loud_and_leaves_no_debris() {
    let bed = bed();
    let (handle, _rx, _hub) = NodeHandle::channel();
    let prov = NodedProvisioner::new(
        crate::agent_provision::test_link(handle.with_forge_repo(&bed.repo_base)).await,
        &bed.runs_root,
    )
    .with_forge_probed(NODE_IDENT, || Err("git probe exploded".into()));
    // PERMANENT: every forge attempt fails with the construction-time reason.
    for run in ["s1:0", "s1:1"] {
        let err = provision_err(prov.provision(&bed.spec(run, &bed.head, false)).await);
        assert!(
            err.contains("unavailable") && err.contains("git probe exploded"),
            "{err}"
        );
    }
    assert_eq!(
        std::fs::read_dir(&bed.runs_root).unwrap().count(),
        0,
        "an unavailable lane mints nothing"
    );
}

#[tokio::test]
async fn a_handle_without_a_forge_repo_base_is_a_clear_error() {
    let bed = bed();
    let (handle, _rx, _hub) = NodeHandle::channel(); // no with_forge_repo
    let prov = NodedProvisioner::new(
        crate::agent_provision::test_link(handle).await,
        &bed.runs_root,
    )
    .with_forge(NODE_IDENT);
    let err = provision_err(prov.provision(&bed.spec("s1:0", &bed.head, false)).await);
    assert!(err.contains("no forge repo base"), "{err}");
}

#[test]
fn validate_coords_refuses_any_push_target_outside_agent_namespace() {
    // #1836's second wall: even if the composer ever named a branch it does
    // not own (a bug, or a corrupt envelope), the provisioner refuses it
    // before a single git command runs.
    let ok_commit = "a".repeat(40);
    validate_coords("app", &ok_commit, "agent/item-7").expect("an agent-scoped branch is fine");
    for outside in [
        "dev",
        "main",
        "feature/x",
        "agent",
        "agent-item-7",
        "/agent/x",
    ] {
        let err = validate_coords("app", &ok_commit, outside).unwrap_err();
        assert!(
            err.contains("agent/") || err.contains("not a safe branch name"),
            "{outside:?} must be refused: {err}"
        );
    }
}

// ---- provision ------------------------------------------------------------

#[tokio::test]
async fn provisions_a_self_contained_clone_at_the_pinned_commit_from_an_odb_only_repo() {
    // the substrate repo has NO checkout (forge materialization shape) — the
    // clone must still materialize the pinned tree.
    let bed = bed();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    assert!(dir.starts_with(&bed.runs_root), "run dir under the W1 root");
    assert!(
        dir.join(".git").is_dir(),
        ".git travels inside the sandbox mount"
    );
    assert!(
        !dir.join(".git/objects/info/alternates").exists(),
        "the run clone never points back at the canonical object store"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let object = format!("{}/{}", &bed.head[..2], &bed.head[2..]);
        let source = std::fs::metadata(bed.repo_dir.join(".git/objects").join(&object)).unwrap();
        let cloned = std::fs::metadata(dir.join(".git/objects").join(object)).unwrap();
        assert_ne!(
            source.ino(),
            cloned.ino(),
            "the sandbox cannot mutate a canonical object through a hardlink"
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("readme.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(git_stdout(&dir, &["rev-parse", "HEAD"]), bed.head);
    // DETACHED: no branch is checked out (no shared-repo ref to hold or move).
    assert_eq!(git_stdout(&dir, &["branch", "--show-current"]), "");
    assert_eq!(
        git_stdout(&dir, &["remote"]),
        "",
        "a run's clone has no configured path back to the canonical repo"
    );
    assert_eq!(
        ws.env().get("DUCKTAPE_RUN_WORKSPACE"),
        Some(&dir.display().to_string())
    );
    assert_eq!(
        ws.env().get("GIT_AUTHOR_NAME").map(String::as_str),
        Some(AGENT_DISPLAY_NAME)
    );
    assert_eq!(
        ws.env().get("GIT_AUTHOR_EMAIL").map(String::as_str),
        Some("quackbot@agents.duck")
    );
    assert_eq!(
        ws.env().get("GIT_COMMITTER_NAME").map(String::as_str),
        Some(NODE_IDENT)
    );
    assert_eq!(
        ws.env().get("GIT_COMMITTER_EMAIL").map(String::as_str),
        Some("node-f00f@nodes.duck")
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn provision_never_touches_shared_repo_refs() {
    let bed = bed();
    // a node-local ref for the work branch already exists (consensus
    // materialized it, or a dead run left it) pointing somewhere else — the
    // detached provision must neither read nor move it: the pin is the base.
    let repo = git2::Repository::open(&bed.repo_dir).unwrap();
    let stale = odb_commit(&repo, Some(&bed.head), "junk.txt", "stale");
    set_ref(&repo, BRANCH, &stale);

    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    assert_eq!(git_stdout(&ws.workdir(), &["rev-parse", "HEAD"]), bed.head);
    assert_eq!(
        ref_oid(&bed.repo_dir, BRANCH).as_deref(),
        Some(stale.as_str()),
        "the shared-repo ref is NOT ours to move — it stays put"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_born_branch_is_forced_to_the_committed_tip_and_pushes_fast_forward() {
    let bed = bed();
    let repo = git2::Repository::open(&bed.repo_dir).unwrap();
    // session so far: the work branch is BORN at `tip` in committed refs...
    let tip = odb_commit(&repo, Some(&bed.head), "work.txt", "session so far\n");
    set_ref(&repo, BRANCH, &tip);
    let bare = bed.snapshot_bare(); // the remote knows main + BRANCH@tip
    // ...but the node-local ref has drifted (leftover from a torn run).
    let drift = odb_commit(&repo, Some(&bed.head), "drift.txt", "node-local drift\n");
    set_ref(&repo, BRANCH, &drift);

    let spec = bed.spec("s2:0", &tip, true);
    let ws = bed
        .provisioner()
        .await
        .provision(&spec)
        .await
        .expect("provision");
    let dir = ws.workdir();
    assert_eq!(
        git_stdout(&dir, &["rev-parse", "HEAD"]),
        tip,
        "the detached checkout sits at the PIN — node-local drift is invisible"
    );

    // continuation run: new work fast-forwards the born branch (CAS holds:
    // remote is exactly at the pinned base).
    std::fs::write(dir.join("more.txt"), "more work\n").unwrap();
    let receipt = ws
        .commit("agent run s2:0", None)
        .await
        .expect("commit+push");
    let new_oid = receipt.output_commit.clone().expect("pushed oid");
    assert_ne!(new_oid, tip);
    assert_eq!(receipt.branch.as_deref(), Some(BRANCH));
    assert_eq!(
        ref_oid(&bare, BRANCH).as_deref(),
        Some(new_oid.as_str()),
        "the remote fast-forwarded to the run's commit"
    );
    // the new commit builds ON the pinned tip — never a rebase, never a force.
    assert_eq!(
        git_stdout(&dir, &["rev-parse", "HEAD^"]),
        tip,
        "the fork base is the pinned commit"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn concurrent_attempts_of_one_item_both_provision_detached() {
    // no branch refs are held, so there is nothing to refuse — both attempts
    // run and the push loop orders whoever finishes second.
    let bed = bed();
    let prov = bed.provisioner().await;
    let a = prov
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("first attempt");
    let b = prov
        .provision(&bed.spec("s1:1", &bed.head, false))
        .await
        .expect("second attempt provisions too — detached HEADs don't contend");
    assert_ne!(a.workdir(), b.workdir());
    a.cleanup().await;
    b.cleanup().await;
}

#[tokio::test]
async fn a_shared_repo_ref_force_move_mid_run_cannot_reparent_the_commit() {
    // THE hazard the detached checkout closes: consensus catch-up force-moves
    // refs/heads/<branch> in the shared repo WHILE the run executes. a
    // worktree holding that branch would commit onto the moved tip with its
    // old tree — silently reverting the interloper, then fast-forwarding past
    // the CAS. detached, the base stays the pin; the interloper is folded in
    // by the FETCH-driven rebase and its content survives.
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, true))
        .await
        .expect("provision");
    let dir = ws.workdir();

    // mid-run: the interloper lands remotely AND catch-up moves the local ref.
    let bare_repo = git2::Repository::open(&bare).unwrap();
    let c2 = odb_commit(
        &bare_repo,
        Some(&bed.head),
        "rival.txt",
        "interloper content\n",
    );
    set_ref(&bare_repo, BRANCH, &c2);
    let local = git2::Repository::open(&bed.repo_dir).unwrap();
    let c2_local = odb_commit(&local, Some(&bed.head), "rival.txt", "interloper content\n");
    assert_eq!(c2_local, c2, "same commit both sides (fixed identities)");
    set_ref(&local, BRANCH, &c2);

    std::fs::write(dir.join("mine.txt"), "the run's work\n").unwrap();
    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("rebase-retry push");
    assert!(receipt.rebased);
    let oid = receipt.output_commit.expect("post-rebase oid");
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(oid.as_str()));
    // the interloper stays an ANCESTOR and its content SURVIVES at the tip —
    // never silently reverted.
    assert_eq!(git_stdout(&dir, &["rev-parse", "HEAD^"]), c2);
    assert_eq!(
        git_stdout(&dir, &["show", "HEAD:rival.txt"]),
        "interloper content"
    );
    assert_eq!(
        git_stdout(&dir, &["show", "HEAD:mine.txt"]),
        "the run's work"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_missing_pinned_commit_fails_provision_before_any_worktree() {
    // committed refs can lead the on-disk pack (materialization lag) — a
    // wrong-base worktree must never be minted.
    let bed = bed();
    let absent = "ab".repeat(20);
    let err = provision_err(
        bed.provisioner()
            .await
            .provision(&bed.spec("s1:0", &absent, false))
            .await,
    );
    assert!(
        err.contains(&absent) && err.contains("not present"),
        "the failure names the missing commit: {err}"
    );
    assert_eq!(std::fs::read_dir(&bed.runs_root).unwrap().count(), 0);
    assert_eq!(worktree_count(&bed.repo_dir), 1, "primary only");
}

#[tokio::test]
async fn a_repo_missing_on_this_node_fails_provision_loudly() {
    let bed = bed();
    let mut spec = bed.spec("s1:0", &bed.head, false);
    spec.source = WorkspaceSource::Forge {
        repo: "ghost".into(),
        item_title: "Fix the flaky gate".into(),
        commit: bed.head.clone(),
        branch: BRANCH.into(),
        branch_born: false,
    };
    let err = provision_err(bed.provisioner().await.provision(&spec).await);
    assert!(
        err.contains("ghost") && err.contains("not materialized"),
        "{err}"
    );
    assert_eq!(std::fs::read_dir(&bed.runs_root).unwrap().count(), 0);
}

// ---- W6 skill mounts ------------------------------------------------------

#[tokio::test]
async fn skill_mounts_land_beside_the_worktree_where_git_can_never_see_them() {
    // the forge lane used to REFUSE these (running without them); it now
    // materializes them exactly like the duckfs lane — at the `-ro` SIBLING,
    // never inside the worktree, where `git add -A` would commit and PUSH the
    // skill trees onto the agent's work branch.
    let bed = bed();
    let (prov, _actor) = bed.skill_provisioner(false).await;
    let ws = prov
        .provision(&bed.skill_spec("s1:0", &bed.head))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let ro = PathBuf::from(ws.env().get("DUCKTAPE_RUN_SKILLS").expect("skills root"));

    assert_eq!(ro, PathBuf::from(format!("{}-ro", dir.display())));
    assert_eq!(
        std::fs::read_to_string(ro.join("qa").join(SKILL_FILE)).unwrap(),
        SKILL_BODY
    );
    // the worktree is UNTOUCHED by the mounts: nothing to stage, nothing to push.
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    assert_eq!(
        git_stdout(&dir, &["status", "--porcelain"]),
        "",
        "the skill trees are invisible to git"
    );

    ws.cleanup().await;
    assert!(!dir.exists());
    assert!(!ro.exists(), "the skill root is the run's debris too");
    assert_eq!(worktree_count(&bed.repo_dir), 1, "primary only");
}

#[tokio::test]
async fn a_failed_skill_mount_checkout_leaves_no_debris() {
    // W5: the run never gets a workspace handle on a failed provision, so the
    // provision unwinds EVERYTHING it made — the partial ro tree AND the
    // worktree it had already added (metadata in the shared repo included).
    let bed = bed();
    let (prov, _actor) = bed.skill_provisioner(true).await;
    let err = provision_err(prov.provision(&bed.skill_spec("s1:0", &bed.head)).await);
    assert!(
        err.contains("chunk not available"),
        "the module's rejection rides out verbatim: {err}"
    );
    assert_eq!(
        std::fs::read_dir(&bed.runs_root).unwrap().count(),
        0,
        "neither the worktree nor the ro root survives"
    );
    assert_eq!(
        worktree_count(&bed.repo_dir),
        1,
        "primary only — metadata pruned"
    );
}

// ---- commit + push --------------------------------------------------------

#[test]
fn commit_message_selection_rejects_identity_spoofing_without_rewriting_the_fallback() {
    let message = select_commit_message(
        Some(
            "Fix the actual bug\r\n\r\nKeep the useful body:\r\n\tindented code survives\r\n\r\n\
             Co-Authored-By: Human <human@example.com>\r\n\
             Co-Authored-By: Old Bot <old@agents.ducktape.local>\r\n\
             Co-Authored-By: Forged via Ducktape <forged@example.com>\r\n\
             Signed-off-by: Forged DCO <dco@example.com>\r\n\
             Reviewed-by: Fake Reviewer <fake@example.com>\r\n\
             Acked-by: Fake Acker <ack@example.com>\r\n\
             Tested-by: Fake Tester <test@example.com>",
        ),
        "Unused Forge title",
    )
    .expect("the Forge title is authoritative");
    assert_eq!(message, "Unused Forge title");

    for trailer in [
        "Reported-by: Human <human@example.com>",
        "Suggested-by: Human <human@example.com>",
        "Co-developed-by: Human <human@example.com>",
        "Author: Human <human@example.com>",
    ] {
        assert!(
            commit_message_candidate(&format!("Useful subject\n\n{trailer}")).is_none(),
            "identity trailer escaped: {trailer}"
        );
    }
}

#[cfg(unix)]
#[test]
fn git_control_sanitization_rejects_nested_object_and_ref_symlinks() {
    use std::os::unix::fs::symlink;

    for relative in ["objects/evil", "refs/heads/evil"] {
        let temp = tempfile::tempdir().unwrap();
        let git_dir = temp.path().join(".git");
        let path = git_dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &path).unwrap();

        let err = sanitize_agent_git_control(temp.path()).unwrap_err();
        assert!(err.contains("contains symlink"), "{relative}: {err}");
    }
}

/// every id consensus admits today is a DNS label, and a label IS the address:
/// `quackbot` → `quackbot@agents.duck`, resolvable straight back to the
/// registry key.
#[test]
fn attribution_addresses_round_trip_a_label_shaped_agent_id() {
    let longest = "x".repeat(63);
    for id in [AGENT, "qa-luna", "a", longest.as_str()] {
        assert!(runs::validate_agent_id(id).is_ok(), "{id}");
        let local = attribution_email_local_part(id);
        assert_eq!(local, id);
        assert!(local.len() <= 63, "{local}");
    }
}

#[test]
fn missing_unsafe_and_oversized_proposals_fall_back_to_the_forge_title() {
    let oversized = "x".repeat(MAX_COMMIT_MESSAGE_BYTES + 1);
    for proposal in [
        None,
        Some(" \r\n "),
        Some("dispatch\u{1f}runs\u{1f}private:0"),
        Some(oversized.as_str()),
    ] {
        let long_name = "🦆".repeat(100);
        let safe_name = sanitize_display_name(&long_name);
        assert!(safe_name.len() <= MAX_DISPLAY_NAME_BYTES);
        assert!(safe_name.is_char_boundary(safe_name.len()));
        let message = select_commit_message(proposal, "Fix the flaky gate")
            .expect("the Forge title recovers an invalid agent proposal");
        assert_eq!(message, "Fix the flaky gate");
        assert!(!message.contains("dispatch") && message.len() <= MAX_COMMIT_MESSAGE_BYTES);
        assert!(message.is_char_boundary(message.len()));
    }

    let err = select_commit_message(
        Some("Fix it\n\nCo-Authored-By: Human <human@example.com>"),
        "invalid\u{1f}Forge title",
    )
    .unwrap_err();
    assert!(
        err.contains("missing or invalid") && !err.contains("Human"),
        "{err:?}"
    );
}

#[test]
fn the_forge_title_owns_the_primary_capture_and_response_is_only_fallback() {
    assert_eq!(
        select_commit_message(
            Some("Final response subject\n\nFinal response body"),
            "Forge title",
        )
        .unwrap(),
        "Forge title"
    );
    assert_eq!(
        select_commit_message(
            Some("Unsafe claim\n\nReviewed-by: Human <human@example.com>"),
            "Forge title",
        )
        .unwrap(),
        "Forge title"
    );
    assert_eq!(
        select_commit_message(Some("invalid\u{1f}message"), "Forge title").unwrap(),
        "Forge title"
    );
    assert_eq!(
        select_commit_message(
            Some("ship it 🦆\n\nThe agent chooses its own style."),
            "Forge title",
        )
        .unwrap(),
        "Forge title"
    );
    assert_eq!(
        select_commit_message(Some("Response fallback\n\nUseful detail"), "").unwrap(),
        "Response fallback\n\nUseful detail"
    );
    assert_eq!(
        select_commit_message(Some("Apply agent changes"), "Exact issue title").unwrap(),
        "Exact issue title",
        "generic response prose must never replace bound item metadata"
    );
}

#[tokio::test]
async fn a_push_lands_and_the_receipt_is_the_forge_output_ref() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    std::fs::write(ws.workdir().join("answer.md"), "the work\n").unwrap();

    let receipt = ws
        .commit(
            "agent run s1:0",
            Some("fix(forge): keep the agent's intent\n\nPreserve the useful explanation."),
        )
        .await
        .expect("commit+push");
    assert!(!receipt.no_changes && receipt.commit_error.is_none());
    assert!(
        !receipt.rebased,
        "an uncontended push never claims a rebase"
    );
    assert_eq!(receipt.source_prefix, format!("forge:{REPO}"));
    assert_eq!(receipt.source_snapshot.as_deref(), Some(bed.head.as_str()));
    assert_eq!(receipt.branch.as_deref(), Some(BRANCH));
    assert_eq!(receipt.output_snapshot, None);
    assert_eq!(receipt.commit_height, None);
    let oid = receipt.output_commit.expect("the new commit oid");
    assert_ne!(oid, bed.head);
    // the push CROSSED: the remote's branch is exactly the receipt's oid,
    // and the verified item title — not response prose or the internal run
    // key — owns the primary capture commit.
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(oid.as_str()));
    assert_eq!(
        git_stdout(&ws.workdir(), &["log", "-1", "--format=%s"]),
        "Fix the flaky gate"
    );
    let body = git_stdout(&ws.workdir(), &["log", "-1", "--format=%B"]);
    assert_eq!(body, "Fix the flaky gate");
    ws.cleanup().await;
}

#[cfg(unix)]
#[tokio::test]
async fn host_git_ignores_agent_installed_hooks_and_filters() {
    let bed = bed();
    bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let sentinel = dir.join("host-git-ran");
    let filter = dir.join("evil-filter.sh");
    install_script(
        &filter,
        &format!("#!/bin/sh\ntouch '{}'\ncat\n", sentinel.display()),
    );
    run_git(
        &dir,
        &["config", "filter.evil.clean", &filter.display().to_string()],
        &[],
    )
    .unwrap();
    std::fs::write(dir.join(".gitattributes"), "answer.md filter=evil\n").unwrap();
    let hook = dir.join(".git/hooks/pre-push");
    install_script(
        &hook,
        &format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    );
    std::fs::write(dir.join("answer.md"), "safe work\n").unwrap();

    ws.commit("agent run s1:0", Some("Publish without host execution"))
        .await
        .expect("commit+push");
    assert!(
        !sentinel.exists(),
        "host Git executed an agent-controlled hook or filter"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_dirty_tree_pushes_with_agent_and_node_identity() {
    let bed = bed();
    bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    std::fs::write(ws.workdir().join("answer.md"), "the work\n").unwrap();
    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("commit+push");
    assert_eq!(receipt.branch.as_deref(), Some(BRANCH));
    // D2: author = the agent (synthetic email), committer = the node identity.
    assert_eq!(
        git_stdout(&ws.workdir(), &["log", "-1", "--format=%an|%ae|%cn|%ce"]),
        format!(
            "{AGENT_DISPLAY_NAME}|{}@agents.duck|{NODE_IDENT}|{NODE_IDENT}@nodes.duck",
            attribution_email_local_part(AGENT)
        )
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_clean_tree_yields_no_changes_and_no_push() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let runtime = ws.workdir().join(provider_host::RUN_RUNTIME_DIR);
    assert_eq!(
        git_stdout(&ws.workdir(), &["remote"]),
        "",
        "a run's clone has no configured path back to the canonical repo"
    );
    std::fs::create_dir_all(runtime.join("provider-config")).unwrap();
    std::fs::write(runtime.join("provider-config/auth.json"), "must-not-push").unwrap();

    let receipt = ws.commit("agent run s1:0", None).await.expect("commit");
    assert!(receipt.no_changes, "a clean tree is a true no_changes");
    assert!(
        !runtime.exists(),
        "provider runtime debris is removed before commit"
    );
    assert_eq!(receipt.source_prefix, format!("forge:{REPO}"));
    assert_eq!(receipt.source_snapshot.as_deref(), Some(bed.head.as_str()));
    assert_eq!(receipt.branch, None, "no push landed");
    assert_eq!(receipt.output_commit, None);
    assert_eq!(
        ref_oid(&bare, BRANCH),
        None,
        "NO push happened — the remote never saw the branch"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn an_agent_created_commit_chain_is_pushed_without_rewriting_it() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let author_email = format!("{}@agents.duck", attribution_email_local_part(AGENT));
    let committer_email = format!("{NODE_IDENT}@nodes.duck");
    let identity = [
        ("GIT_AUTHOR_NAME", AGENT_DISPLAY_NAME),
        ("GIT_AUTHOR_EMAIL", author_email.as_str()),
        ("GIT_COMMITTER_NAME", NODE_IDENT),
        ("GIT_COMMITTER_EMAIL", committer_email.as_str()),
    ];
    std::fs::write(dir.join("one.txt"), "one\n").unwrap();
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    run_git(&dir, &["commit", "-m", "First agent decision"], &identity).unwrap();
    let first = git_stdout(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("two.txt"), "two\n").unwrap();
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    run_git(
        &dir,
        &[
            "commit",
            "-m",
            "second choice",
            "-m",
            "The agent owns this body.",
        ],
        &identity,
    )
    .unwrap();
    let second = git_stdout(&dir, &["rev-parse", "HEAD"]);

    let receipt = ws
        .commit(
            "agent run s1:0",
            Some("This response must not squash or rename committed work"),
        )
        .await
        .expect("push");
    assert!(!receipt.no_changes);
    assert_eq!(receipt.output_commit.as_deref(), Some(second.as_str()));
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(second.as_str()));
    assert_eq!(git_stdout(&dir, &["rev-parse", "HEAD^"]), first);
    assert_eq!(
        git_stdout(&dir, &["log", "-2", "--format=%s"]),
        "second choice\nFirst agent decision"
    );
    assert_eq!(
        git_stdout(&dir, &["log", "-1", "--format=%B"]),
        "second choice\n\nThe agent owns this body."
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn uncommitted_work_is_captured_on_top_of_the_agents_commit() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let author_email = format!("{}@agents.duck", attribution_email_local_part(AGENT));
    let committer_email = format!("{NODE_IDENT}@nodes.duck");
    let identity = [
        ("GIT_AUTHOR_NAME", AGENT_DISPLAY_NAME),
        ("GIT_AUTHOR_EMAIL", author_email.as_str()),
        ("GIT_COMMITTER_NAME", NODE_IDENT),
        ("GIT_COMMITTER_EMAIL", committer_email.as_str()),
    ];
    std::fs::write(dir.join("committed.txt"), "committed\n").unwrap();
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    run_git(&dir, &["commit", "-m", "Keep this commit"], &identity).unwrap();
    let agent_commit = git_stdout(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(dir.join("final.txt"), "uncommitted\n").unwrap();

    let receipt = ws
        .commit(
            "agent run s1:0",
            Some("Capture the final edit\n\nKeep the earlier commit intact."),
        )
        .await
        .expect("commit+push");
    let output = receipt.output_commit.expect("capture commit");
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(output.as_str()));
    assert_eq!(git_stdout(&dir, &["rev-parse", "HEAD^"]), agent_commit);
    assert_eq!(
        git_stdout(&dir, &["log", "-2", "--format=%s"]),
        "Fix the flaky gate\nKeep this commit"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn no_agent_message_or_forge_title_never_pushes_synthetic_history() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let mut spec = bed.spec("s1:0", &bed.head, false);
    let WorkspaceSource::Forge { item_title, .. } = &mut spec.source else {
        unreachable!()
    };
    // an EMPTY title is the "no usable forge title" case now that the field is
    // required: it fails the commit-message candidate and falls to the prose.
    *item_title = String::new();
    let ws = bed
        .provisioner()
        .await
        .provision(&spec)
        .await
        .expect("provision");
    std::fs::write(ws.workdir().join("answer.md"), "real work\n").unwrap();

    let err = ws.commit("agent run s1:0", None).await.unwrap_err();
    assert!(err.contains("missing or invalid"), "{err}");
    assert_eq!(
        ref_oid(&bare, BRANCH),
        None,
        "the runtime must not push a message it invented"
    );
    ws.cleanup().await;
}

/// the pushed history from the pin up: one line per commit, `author\0committer\0subject`,
/// oldest first.
fn pushed_history(bare: &Path, base: &str) -> Vec<String> {
    let range = format!("{base}..refs/heads/{BRANCH}");
    git_stdout(
        bare,
        &["log", "--reverse", "--format=%an%x00%cn%x00%s", &range],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

#[tokio::test]
async fn a_spoofed_identity_costs_the_history_not_the_work() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    std::fs::write(dir.join("safe.txt"), "must survive\n").unwrap();
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    run_git(
        &dir,
        &["commit", "-m", "dispatch\u{1f}runs\u{1f}private-hash:0"],
        &[
            ("GIT_AUTHOR_NAME", "attacker"),
            ("GIT_AUTHOR_EMAIL", "attacker@example.com"),
            ("GIT_COMMITTER_NAME", "attacker"),
            ("GIT_COMMITTER_EMAIL", "attacker@example.com"),
        ],
    )
    .unwrap();

    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("commit+push");
    let output = receipt.output_commit.expect("capture commit");
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(output.as_str()));
    assert_eq!(
        pushed_history(&bare, &bed.head),
        [format!(
            "{AGENT_DISPLAY_NAME}\0{NODE_IDENT}\0Fix the flaky gate"
        )],
        "the attacker's commit is gone; the node's capture is the only one"
    );
    assert_eq!(
        git_stdout(&bare, &["show", &format!("{output}:safe.txt")]),
        "must survive"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn an_identity_trailer_costs_the_history_not_the_work() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let author_email = format!("{}@agents.duck", attribution_email_local_part(AGENT));
    let committer_email = format!("{NODE_IDENT}@nodes.duck");
    let identity = [
        ("GIT_AUTHOR_NAME", AGENT_DISPLAY_NAME),
        ("GIT_AUTHOR_EMAIL", author_email.as_str()),
        ("GIT_COMMITTER_NAME", NODE_IDENT),
        ("GIT_COMMITTER_EMAIL", committer_email.as_str()),
    ];
    std::fs::write(dir.join("first.txt"), "first\n").unwrap();
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    run_git(&dir, &["commit", "-m", "Keep this commit"], &identity).unwrap();
    std::fs::write(dir.join("second.txt"), "second\n").unwrap();
    run_git(&dir, &["add", "-A"], &[]).unwrap();
    run_git(
        &dir,
        &[
            "commit",
            "-m",
            "Redesign the card\n\nCo-Authored-By: Someone <someone@example.com>",
        ],
        &identity,
    )
    .unwrap();
    std::fs::write(dir.join("third.txt"), "uncommitted\n").unwrap();

    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("commit+push");
    let output = receipt.output_commit.expect("capture commit");
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(output.as_str()));
    assert_eq!(
        pushed_history(&bare, &bed.head),
        [format!(
            "{AGENT_DISPLAY_NAME}\0{NODE_IDENT}\0Fix the flaky gate"
        )],
        "one refused commit drops the whole agent history, kept commits included"
    );
    let body = git_stdout(&bare, &["log", "-1", "--format=%B", &output]);
    assert!(!body.contains("Co-Authored-By"), "{body}");
    for (file, content) in [
        ("first.txt", "first"),
        ("second.txt", "second"),
        ("third.txt", "uncommitted"),
    ] {
        assert_eq!(
            git_stdout(&bare, &["show", &format!("{output}:{file}")]),
            content
        );
    }
    ws.cleanup().await;
}

#[tokio::test]
async fn agent_history_must_descend_from_the_pinned_forge_commit() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let tree = git_stdout(&dir, &["rev-parse", "HEAD^{tree}"]);
    let author_email = format!("{}@agents.duck", attribution_email_local_part(AGENT));
    let committer_email = format!("{NODE_IDENT}@nodes.duck");
    let identity = [
        ("GIT_AUTHOR_NAME", AGENT_DISPLAY_NAME),
        ("GIT_AUTHOR_EMAIL", author_email.as_str()),
        ("GIT_COMMITTER_NAME", NODE_IDENT),
        ("GIT_COMMITTER_EMAIL", committer_email.as_str()),
    ];
    let unrelated = run_git(
        &dir,
        &["commit-tree", &tree, "-m", "Unrelated history"],
        &identity,
    )
    .unwrap();
    let disguise = run_git(
        &dir,
        &[
            "commit-tree",
            &tree,
            "-p",
            &bed.head,
            "-m",
            "Replacement descendant",
        ],
        &identity,
    )
    .unwrap();
    run_git(&dir, &["replace", &unrelated, &disguise], &[]).unwrap();
    std::fs::write(
        dir.join(".git/info/grafts"),
        format!("{unrelated} {}\n", bed.head),
    )
    .unwrap();
    run_git(&dir, &["reset", "--hard", &unrelated], &[]).unwrap();

    let err = ws.commit("agent run s1:0", None).await.unwrap_err();
    assert!(err.contains("does not descend from the pinned"), "{err}");
    assert!(!dir.join(".git/info/grafts").exists());
    assert_eq!(
        ref_oid(&bare, BRANCH),
        None,
        "unrelated history never pushes"
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_concurrent_advance_is_rebased_under_the_runs_work_and_pushed() {
    // "we replicate all of git actions": a concurrent advance is an ORDERING
    // problem — fetch, rebase, push; both commits land as linear history.
    let bed = bed();
    let bare = bed.snapshot_bare();
    // someone advanced the branch on the remote AFTER this run's base was
    // pinned: mint C2 (a child of head) straight in the bare repo.
    let bare_repo = git2::Repository::open(&bare).unwrap();
    let c2 = odb_commit(
        &bare_repo,
        Some(&bed.head),
        "rival.txt",
        "concurrent work\n",
    );
    set_ref(&bare_repo, BRANCH, &c2);

    let spec = bed.spec("s1:0", &bed.head, true);
    let ws = bed
        .provisioner()
        .await
        .provision(&spec)
        .await
        .expect("provision");
    let dir = ws.workdir();
    std::fs::write(dir.join("mine.txt"), "forked work\n").unwrap();

    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("rebase-retry push");
    assert!(
        receipt.rebased,
        "the receipt says the base moved under the work"
    );
    assert!(!receipt.no_changes && receipt.commit_error.is_none());
    assert_eq!(receipt.branch.as_deref(), Some(BRANCH));
    let oid = receipt.output_commit.expect("post-rebase oid");
    // linear history: the interloper is now the run commit's PARENT …
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(oid.as_str()));
    assert_eq!(git_stdout(&dir, &["rev-parse", "HEAD^"]), c2);
    // … and the rebase preserved the agent author + node committer (D2).
    assert_eq!(
        git_stdout(&dir, &["log", "-1", "--format=%an|%ae|%cn|%ce"]),
        format!(
            "{AGENT_DISPLAY_NAME}|{}@agents.duck|{NODE_IDENT}|{NODE_IDENT}@nodes.duck",
            attribution_email_local_part(AGENT)
        )
    );
    ws.cleanup().await;
}

#[tokio::test]
async fn a_concurrent_advance_preserves_agent_merge_topology_and_messages() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let bare_repo = git2::Repository::open(&bare).unwrap();
    let rival = odb_commit(
        &bare_repo,
        Some(&bed.head),
        "rival.txt",
        "concurrent work\n",
    );
    set_ref(&bare_repo, BRANCH, &rival);

    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, true))
        .await
        .expect("provision");
    let dir = ws.workdir();
    let author_email = format!("{}@agents.duck", attribution_email_local_part(AGENT));
    let committer_email = format!("{NODE_IDENT}@nodes.duck");
    let identity = [
        ("GIT_AUTHOR_NAME", AGENT_DISPLAY_NAME),
        ("GIT_AUTHOR_EMAIL", author_email.as_str()),
        ("GIT_COMMITTER_NAME", NODE_IDENT),
        ("GIT_COMMITTER_EMAIL", committer_email.as_str()),
    ];

    run_git(&dir, &["switch", "-c", "agent-topic"], &[]).unwrap();
    std::fs::write(dir.join("topic.txt"), "topic work\n").unwrap();
    run_git(&dir, &["add", "topic.txt"], &[]).unwrap();
    run_git(&dir, &["commit", "-m", "Agent topic message"], &identity).unwrap();

    run_git(&dir, &["switch", "--detach", &bed.head], &[]).unwrap();
    std::fs::write(dir.join("linear.txt"), "linear work\n").unwrap();
    run_git(&dir, &["add", "linear.txt"], &[]).unwrap();
    run_git(&dir, &["commit", "-m", "Agent linear message"], &identity).unwrap();
    run_git(
        &dir,
        &[
            "merge",
            "--no-ff",
            "agent-topic",
            "-m",
            "Agent merge message",
        ],
        &identity,
    )
    .unwrap();

    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("merge-preserving rebase-retry push");
    assert!(receipt.rebased);
    let oid = receipt.output_commit.expect("post-rebase oid");
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(oid.as_str()));
    assert_eq!(
        git_stdout(&dir, &["rev-list", "--parents", "-1", "HEAD"])
            .split_whitespace()
            .count(),
        3,
        "the rebased head remains a two-parent merge"
    );
    assert!(
        run_git(&dir, &["merge-base", "--is-ancestor", &rival, "HEAD"], &[]).is_ok(),
        "the concurrent remote tip is retained below the recreated merge"
    );
    let messages = git_stdout(&dir, &["log", "--format=%s", &format!("{rival}..HEAD")]);
    for message in [
        "Agent merge message",
        "Agent linear message",
        "Agent topic message",
    ] {
        assert_eq!(
            messages.lines().filter(|line| *line == message).count(),
            1,
            "{message:?} is preserved exactly once: {messages}"
        );
    }
    assert_eq!(messages.lines().count(), 3, "no commit was flattened away");
    for line in git_stdout(
        &dir,
        &["log", "--format=%an|%ae|%cn|%ce", &format!("{rival}..HEAD")],
    )
    .lines()
    {
        assert_eq!(
            line,
            format!("{AGENT_DISPLAY_NAME}|{author_email}|{NODE_IDENT}|{committer_email}")
        );
    }
    ws.cleanup().await;
}

#[tokio::test]
async fn an_identical_concurrent_patch_keeps_the_agent_attribution_commit() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    let bare_repo = git2::Repository::open(&bare).unwrap();
    let c2 = odb_commit(&bare_repo, Some(&bed.head), "same.txt", "identical work\n");
    set_ref(&bare_repo, BRANCH, &c2);

    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, true))
        .await
        .expect("provision");
    let dir = ws.workdir();
    std::fs::write(dir.join("same.txt"), "identical work\n").unwrap();

    let receipt = ws
        .commit("agent run s1:0", None)
        .await
        .expect("rebase-retry push");
    assert!(receipt.rebased);
    let oid = receipt.output_commit.expect("output commit");
    assert_ne!(
        oid, c2,
        "the unrelated upstream commit is not the receipt output"
    );
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(oid.as_str()));
    assert_eq!(git_stdout(&dir, &["rev-parse", "HEAD^"]), c2);
    assert_eq!(
        git_stdout(&dir, &["log", "-1", "--format=%an|%ae|%s"]),
        format!(
            "{AGENT_DISPLAY_NAME}|{}@agents.duck|Fix the flaky gate",
            attribution_email_local_part(AGENT)
        )
    );
    let body = git_stdout(&dir, &["log", "-1", "--format=%B"]);
    assert_eq!(body, "Fix the flaky gate");
    ws.cleanup().await;
}

#[tokio::test]
async fn a_rebase_conflict_aborts_cleanly_and_degrades() {
    let bed = bed();
    let bare = bed.snapshot_bare();
    // the interloper and the run edit the SAME lines of readme.md.
    let bare_repo = git2::Repository::open(&bare).unwrap();
    let c2 = odb_commit(&bare_repo, Some(&bed.head), "readme.md", "rival edit\n");
    set_ref(&bare_repo, BRANCH, &c2);

    let spec = bed.spec("s1:0", &bed.head, true);
    let ws = bed
        .provisioner()
        .await
        .provision(&spec)
        .await
        .expect("provision");
    let dir = ws.workdir();
    std::fs::write(dir.join("readme.md"), "my conflicting edit\n").unwrap();

    let err = ws.commit("agent run s1:0", None).await.unwrap_err();
    assert!(
        err.contains("rebase conflict") && err.contains(BRANCH),
        "ONLY a genuine conflict degrades, naming the branch: {err}"
    );
    // aborted cleanly: no mid-rebase state left behind …
    let rebase_dir = git_stdout(&dir, &["rev-parse", "--git-path", "rebase-merge"]);
    let rebase_path = Path::new(&rebase_dir);
    let rebase_abs = if rebase_path.is_absolute() {
        rebase_path.to_path_buf()
    } else {
        dir.join(rebase_path)
    };
    assert!(
        !rebase_abs.exists(),
        "no rebase-merge debris at {rebase_dir}"
    );
    // … the interloper's tip stays branch head …
    assert_eq!(ref_oid(&bare, BRANCH).as_deref(), Some(c2.as_str()));
    // … and the pool maps exactly this Err to commit_failed + Degraded.
    let receipt = WorkspaceReceipt::commit_failed(&spec, err);
    assert_eq!(receipt.branch.as_deref(), Some(BRANCH));
    assert!(receipt.commit_error.is_some() && !receipt.no_changes);
    assert_eq!(receipt.output_commit, None);
    ws.cleanup().await;
}

#[tokio::test]
async fn a_remote_that_always_rejects_exhausts_the_bounded_retries() {
    let bed = bed();
    // the branch must be born on the remote so mid-loop fetch/rebase succeed
    // and the BOUND — not a fetch error — is what ends the loop.
    let repo = git2::Repository::open(&bed.repo_dir).unwrap();
    set_ref(&repo, BRANCH, &bed.head);
    let bare = bed.snapshot_bare();
    // a pre-receive hook that logs every push attempt and declines it.
    let hook = bare.join("hooks").join("pre-receive");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    install_script(&hook, "#!/bin/sh\necho x >> hook.log\nexit 1\n");

    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, true))
        .await
        .expect("provision");
    std::fs::write(ws.workdir().join("mine.txt"), "work\n").unwrap();

    let err = ws.commit("agent run s1:0", None).await.unwrap_err();
    assert!(
        err.contains("after 3 attempts") && err.contains("pre-receive"),
        "the bound ends the loop carrying the real reject: {err}"
    );
    let pushes = std::fs::read_to_string(bare.join("hook.log")).unwrap();
    assert_eq!(
        pushes.lines().count(),
        3,
        "exactly PUSH_ATTEMPTS pushes, no spin"
    );
    ws.cleanup().await;
}

// ---- cleanup (W5) ---------------------------------------------------------

#[tokio::test]
async fn cleanup_removes_the_worktree_and_its_metadata_even_uncommitted() {
    // the unwind-path shape: the pool calls cleanup() unconditionally —
    // panicking providers included — so a dirty, never-committed tree must
    // go quietly, metadata and all.
    let bed = bed();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    let dir = ws.workdir();
    std::fs::write(dir.join("scratch.txt"), "uncommitted\n").unwrap();

    ws.cleanup().await;
    assert!(!dir.exists(), "the run dir is gone");
    assert_eq!(
        worktree_count(&bed.repo_dir),
        1,
        "primary only — the worktree metadata was pruned"
    );
    // idempotent: a second cleanup (or one after external deletion) is quiet.
    ws.cleanup().await;
    assert!(!dir.exists());
}

#[tokio::test]
async fn cleanup_after_a_successful_push_leaves_only_the_branch_ref() {
    let bed = bed();
    bed.snapshot_bare();
    let ws = bed
        .provisioner()
        .await
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    std::fs::write(ws.workdir().join("answer.md"), "done\n").unwrap();
    let receipt = ws.commit("agent run s1:0", None).await.expect("push");
    let dir = ws.workdir();
    ws.cleanup().await;
    assert!(!dir.exists());
    assert_eq!(worktree_count(&bed.repo_dir), 1, "primary only");
    let _ = receipt;
    // detached lane: the work branch lives only on the remote — the shared
    // repo never grew a local ref for it.
    assert_eq!(ref_oid(&bed.repo_dir, BRANCH), None);
}

// ---- push credential is resolved at push time, never latched at provision --

/// A generic module/blob endpoint captures the credential used at upload time.
async fn spawn_credential_capturing_remote(
    captured: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
) -> std::net::SocketAddr {
    let upload = move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
        let captured = captured.clone();
        async move {
            use sha2::{Digest as _, Sha256};
            captured.lock().unwrap().push(
                headers
                    .get(crate::admin::ADMIN_TOKEN_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
            );
            axum::Json(serde_json::json!({"digest":format!("{:x}",Sha256::digest(&body))}))
        }
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let router = axum::Router::new()
        .route(
            "/v1/query",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"refs":[]})) }),
        )
        .route("/v1/files/blob", axum::routing::post(upload))
        .route(
            "/v1/submit",
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let is_publication = body["target"] == "forge";
                    if is_publication {
                        let required = body["required_blob"]
                            .as_str()
                            .expect("publication declares the uploaded blob");
                        let digest = duckfs_core::from_hex_32(required).unwrap();
                        let message: forge::ForgeMsg =
                            serde_json::from_value(body["payload"].clone()).unwrap();
                        let forge::ForgeMsg::PushRefs { pack_digest, .. } = message else {
                            panic!("expected publication");
                        };
                        assert_eq!(pack_digest.as_deref(), Some(digest.as_slice()));
                    }
                    axum::Json(super::super::plane_tests::committed_block())
                },
            ),
        );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    address
}

#[tokio::test]
async fn a_restart_mid_run_never_leaves_the_push_presenting_the_provision_time_token() {
    let bed = bed();
    let workspace = tempfile::tempdir().unwrap();
    // T1: the credential this node's `admin.token` held when the run was
    // provisioned.
    let provision_time_token = crate::admin::mint_operator_token(workspace.path())
        .expect("mint the provision-time credential");

    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let remote = spawn_credential_capturing_remote(captured.clone()).await;

    let link = NodeLink::new(format!("http://{remote}"))
        .with_workspace_credential(workspace.path())
        .with_forge_repo(bed.repo_base.clone());
    let prov = NodedProvisioner::new(link, &bed.runs_root).with_forge(NODE_IDENT);

    let ws = prov
        .provision(&bed.spec("s1:0", &bed.head, false))
        .await
        .expect("provision");
    std::fs::write(ws.workdir().join("answer.md"), "the work\n").unwrap();

    // the node restarts mid-run: `admin.token` is re-minted (T1 is now dead).
    let post_restart_token = crate::admin::mint_operator_token(workspace.path())
        .expect("mint the post-restart credential");
    assert_ne!(provision_time_token, post_restart_token);

    // commit_blocking's push presents whatever the SAME NodeLink reads right
    // now — never the value `provision` observed, because nothing captured
    // it into an owned field.
    ws.commit("agent run s1:0", None)
        .await
        .expect("generic module publication");

    let presented = captured.lock().unwrap().clone();
    assert!(
        !presented.is_empty(),
        "the fake node never saw a blob upload"
    );
    assert!(
        presented
            .iter()
            .all(|t| t.as_deref() == Some(post_restart_token.as_str())),
        "every push attempt must present the CURRENT credential, never the \
         provision-time one: {presented:?}"
    );
}

struct GitPublication(String);
impl super::super::forge_publication::Publication for GitPublication {
    fn push(&self, repo: &str, run: &Path, branch: &str) -> Result<(), String> {
        run_git(
            run,
            &[
                "push",
                &format!("{}/{repo}", self.0),
                &format!("HEAD:refs/heads/{branch}"),
            ],
            &[],
        )
        .map(|_| ())
    }
    fn fetch(&self, repo: &str, run: &Path, branch: &str) -> Result<(), String> {
        run_git(
            run,
            &[
                "fetch",
                &format!("{}/{repo}", self.0),
                &format!("refs/heads/{branch}"),
            ],
            &[],
        )
        .map(|_| ())
    }
}
impl NodedProvisioner {
    fn with_test_publication(mut self, base: String) -> Self {
        self.forge.as_mut().unwrap().publication = std::sync::Arc::new(GitPublication(base));
        self
    }
}
