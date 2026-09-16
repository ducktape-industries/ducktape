//! the REAL [`WorkspaceProvisioner`] for portable (v1) agent runs: one
//! per-run workspace under a D7-validated root, materialized from whichever
//! source the run's envelope pinned.
//!
//! two lanes, dispatched on [`WorkspaceSource`]:
//! - **duckfs** ([`duckfs`]): checkout / commit a duckfs subtree over the
//!   node's `/v1` surface ([`crate::node_link::NodeLink`]).
//! - **forge** ([`forge`]): a git WORKTREE of a node-local forge repo at the
//!   run's pinned commit, committed with agent authorship and pushed back
//!   through this node's own loopback smart-HTTP lane so the branch move
//!   settles through consensus (wire contract §4). configured per binary via
//!   [`NodedProvisioner::with_forge`]; unconfigured or unusable (no repo
//!   base, no http surface, no worktree-capable host `git`) the lane fails
//!   each forge attempt LOUDLY while duckfs runs are untouched.
//!
//! this lives in the noded LIB crate — the only place `duckfs-client` (the
//! checkout/commit engine) and the node's `/v1` lane are both reachable, the
//! reachability wall compute-service cannot cross. It runs in the COMPUTE
//! DAEMON's process, not the node's: every consensus read and write below goes
//! over `/v1`, so the provisioner has no in-process dependency on the node at
//! all.
//!
//! whichever lane materializes it, every run is handed the same TOOL PLANE
//! ([`run_env`] + [`tool_path_entries`]): the bin dir of the running binary on
//! `PATH` (where `ducktape mcp` ships), the node's http base as `DUCKTAPE_NODE`,
//! and its agent id as `DUCKTAPE_RUN_AGENT`. that is enough for the MCP server
//! — which the runner CLI spawns OUTSIDE the agent's sandbox — to find the node
//! and know who it acts for; the record itself is never in the env (see
//! [`run_env`]).
//!
//! D7 (isolation floor): the per-run dir is minted under [`agent_runs_root`],
//! a root VALIDATED at boot to be OUTSIDE `<storage>` — so a `..` from a
//! checkout can NOT reach `user.key`, the node keys, qmdb, the blobstore, or
//! forge's git substrate. the managed `/v1/fs/workspaces` root stays under
//! `<storage>`; this is a distinct, relocated root for live agent runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use compute_service::{
    ProvisionedWorkspace, RoMount, SkillDoc, WorkspaceProvisioner, WorkspaceSource, WorkspaceSpec,
    assemble_context_doc, parse_skill_md,
};
use duckfs_client::checkout::{CheckoutOptions, checkout_with};
use runs::is_skill_mount_name;

use crate::node_link::NodeLink;
use provider_host::OperatorCredential;

mod duckfs;
mod forge;
mod forge_publication;
mod session;

/// Serve `handle`'s own `/v1` router on loopback and return a [`NodeLink`] to
/// it — the test seam for everything the provisioner does over http.
///
/// It is the REAL router over the test's own fake actor, not a stub: the
/// provisioner's transport is exactly what a live daemon uses, while the tests
/// keep asserting on the `NodeCommand`s that reach the actor. Nothing here is
/// compiled into a shipping binary.
#[cfg(test)]
pub(crate) async fn test_link(handle: crate::NodeHandle) -> NodeLink {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind a loopback test surface");
    let address = listener
        .local_addr()
        .expect("read the test surface address");
    let forge_repo = handle.forge_repo.clone();
    // the provisioner WRITES duckfs, and every mutating route now wants a
    // credential — so the fake node carries one and the link presents it,
    // exactly as a real daemon reads it out of the node's workspace.
    //
    // the workspace is KEPT (not a scoped `TempDir`): the link re-reads
    // `admin.token` per attach, so a dir dropped at the end of this function
    // would leave every later request uncredentialed.
    let workspace = tempfile::tempdir().expect("a test workspace").keep();
    let operator_token = crate::admin::mint_operator_token(&workspace)
        .expect("mint the test node's operator credential");
    let handle = handle.with_admin(crate::AdminConfig {
        operator_token: Some(operator_token.clone()),
        ..crate::AdminConfig::default()
    });
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            crate::router(handle).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });
    let link = NodeLink::new(format!("http://{address}")).with_workspace_credential(&workspace);
    match forge_repo {
        Some(base) => link.with_forge_repo(base),
        None => link,
    }
}

#[cfg(test)]
#[path = "agent_provision/plane_tests.rs"]
mod plane_tests;

// the composer → provisioner → consensus id boundary, crossed end to end: the
// one test that fails when a session bind names a run `runs` cannot resolve.
#[cfg(test)]
#[path = "agent_provision/session_boundary_tests.rs"]
mod session_boundary_tests;

/// the D7 relocation lever: the root per-run agent workspaces are minted
/// under. MUST be outside `<storage>` — VALIDATED here at boot, never trusted.
/// `DUCKTAPE_AGENT_RUNS_ROOT` overrides the base (operators point it at an
/// isolated volume). the default is the system temp tree, the same safe
/// scratch tree `CliProvider`'s fallback workdir already uses.
///
/// the returned root is salted with a hash of THIS node's storage path, so
/// co-located nodes (fleet tiles, multi-node test boxes) never share a
/// run-dir tree — one node's W5 cleanup must never be able to delete a
/// sibling process's in-flight checkout.
pub fn agent_runs_root(storage: &Path) -> Result<PathBuf, String> {
    // Default to a sibling of the storage dir (`<workspace>/agent-runs`): on the
    // SAME real disk as storage — never a memory-backed `/tmp` that consumes RAM
    // and that the codex CLI refuses to place its helper binaries under — while
    // still OUTSIDE the storage tree the D7 guard forbids. The env override wins
    // for hosts that want the run tree elsewhere.
    let base = std::env::var_os("DUCKTAPE_AGENT_RUNS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| storage.parent().unwrap_or(storage).join("agent-runs"));
    runs_root_under(base, storage)
}

/// the testable core of [`agent_runs_root`]: salt `base` per-storage, create
/// it, and REFUSE a root inside `<storage>` (D7 is a MUST, not a convention).
fn runs_root_under(base: PathBuf, storage: &Path) -> Result<PathBuf, String> {
    let digest = duckfs_core::objects::object_id(
        duckfs_core::objects::Kind::Chunk,
        storage.to_string_lossy().as_bytes(),
    );
    let salt: String = duckfs_core::to_hex(&digest).chars().take(16).collect();
    let root = base.join(salt);
    std::fs::create_dir_all(&root).map_err(|e| {
        format!(
            "agent runs root {} could not be created: {e}",
            root.display()
        )
    })?;
    // D7 (MUST): the run tree may never live under <storage> — a `..` from a
    // checkout would reach user.key/node keys/qmdb/blobstore. canonicalize
    // both sides so symlinks/relative paths cannot dodge the check.
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("agent runs root {}: {e}", root.display()))?;
    let canon_storage = storage
        .canonicalize()
        .unwrap_or_else(|_| storage.to_path_buf());
    if canon_root.starts_with(&canon_storage) {
        return Err(format!(
            "agent runs root {} is inside the node storage tree {} — D7 forbids \
             this; set DUCKTAPE_AGENT_RUNS_ROOT to a directory outside it",
            canon_root.display(),
            canon_storage.display()
        ));
    }
    Ok(root)
}

/// a bounded, collision-free `[a-z0-9]` dir name derived from the FULL run_id
/// (`"{saga_id}:{attempt}"`). the SHA-256 tail keys the dir on the ENTIRE
/// run_id — INCLUDING the attempt — so distinct attempts of one saga never
/// share a checkout dir. this matters because a re-lease spawns a NEW attempt
/// WITHOUT cancelling the still-running prior one (agent runs are minutes-long,
/// lease windows shorter), so two attempts can execute concurrently; distinct
/// dirs keep them from interleaving writes / racing commits / cleaning up each
/// other's tree. a readable alnum prefix aids debugging but is NEVER the
/// discriminator, and the id is never trusted as a raw path component (no `.`,
/// no `/`, so a per-run dir can never escape the root).
fn run_slug(run_id: &str) -> String {
    // reuse duckfs's content-address hash (no new dep): a domain-separated
    // sha-256 over the FULL run_id → a stable 24-hex tail keyed on the entire
    // id, attempt included.
    let digest =
        duckfs_core::objects::object_id(duckfs_core::objects::Kind::Chunk, run_id.as_bytes());
    let hash: String = duckfs_core::to_hex(&digest).chars().take(24).collect();
    let prefix: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(24)
        .collect();
    format!("{prefix}{hash}")
}

/// a W6 skill mount subpath is consensus-supplied data used as ONE host
/// directory name — never a path. `runs::validate_skills` now enforces this
/// exact shape at consensus time too, so a bad name is refused before it is
/// ever committed; this call stays as the trust boundary of last resort (an
/// older committed record, or a bug in the consensus-side check, must not
/// let a `..` or `a/b` name escape the ro root). ONE predicate,
/// [`runs::is_skill_mount_name`], gates both sides so they cannot drift.
fn mount_dir_name(subpath: &str) -> Result<(), String> {
    if is_skill_mount_name(subpath) {
        Ok(())
    } else {
        Err(format!(
            "skill mount subpath {subpath:?} is not a safe directory name \
             (want [a-zA-Z0-9._-]+, not `.` or `..`)"
        ))
    }
}

/// this node's OWN http base — `http://<host>:<port>`, derived from its listen
/// address, with a wildcard bind rewritten to the SAME family's loopback
/// (`0.0.0.0` → `127.0.0.1`, `[::]` → `[::1]` — a bindv6only `[::]` listener
/// refuses v4 loopback dials). the base must be a CONNECTABLE host: a run's
/// tool plane dials it back (`DUCKTAPE_NODE`), as does module publication.
/// `None` means there is no HTTP surface to dial.
pub fn node_http_base(http_listen: Option<&str>) -> Option<String> {
    let listen = http_listen?;
    let base = match listen.parse::<std::net::SocketAddr>() {
        Ok(addr) if addr.ip().is_unspecified() => {
            let loopback = if addr.is_ipv6() { "[::1]" } else { "127.0.0.1" };
            format!("{loopback}:{}", addr.port())
        }
        Ok(addr) => addr.to_string(),
        // not a socket address (hostname:port) — trust the operator's string.
        Err(_) => listen.to_string(),
    };
    Some(format!("http://{base}"))
}

/// the tool plane's PATH entry: the directory holding the CURRENTLY-RUNNING
/// binary. `ducktape mcp` ships beside `noded`/`node`, and the runner CLI
/// (codex/claude) spawns the MCP server by bare command name. The sandbox
/// stages its commands as a read-only guest asset and translates this PATH entry.
///
/// a failing `current_exe` (an exotic platform, a deleted/replaced binary)
/// degrades to NO entry rather than failing the run: the agent still runs,
/// just without the tool plane. never the other way round.
fn tool_path_entries() -> Vec<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .into_iter()
        .collect()
}

/// the run child's environment, shared by both lanes: where its writable tree
/// is, where its W6 skill trees are, which node its tools dial, and which agent
/// it is acting for.
///
/// `DUCKTAPE_NODE` is deliberately the SAME variable `ducktape fs` reads — one
/// name for "the node this process talks to", so every Ducktape tool a run
/// spawns, including the MCP server inside the guest, finds the node through
/// its read tunnel without a second convention. A node with
/// no http surface has nothing to name, so the var is simply absent.
///
/// `DUCKTAPE_RUN_AGENT` is the run's IDENTITY, and ONLY that. the model's
/// record is read back from the COMMITTED agent registry by whoever holds this
/// id; copying it into the env would mint a second, unversioned copy that
/// drifts from the record it came from the moment the registry moves. the
/// committed record is the one truth.
///
/// `DUCKTAPE_RUN_ACTION_URL` + `DUCKTAPE_RUN_ACTION_TOKEN` + `DUCKTAPE_RUN_ID`
/// are the write half of the tool plane. The endpoint signs only the two Runs
/// messages scoped to this live run; the private key never enters child env.
///
/// `DUCKTAPE_RUN_ID` comes from [`WorkspaceSpec::agent`]: the committed model
/// run ID used for both the read ceiling and interactive writes. The host's
/// `WorkspaceSpec::run_id` names the on-disk attempt directory instead.
fn run_env(
    dir: &Path,
    ro_dir: Option<&Path>,
    node_url: Option<&str>,
    spec: &WorkspaceSpec,
    session: Option<&session::RunSession>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("DUCKTAPE_RUN_WORKSPACE".into(), dir.display().to_string());
    if let Some(ro) = ro_dir {
        // the consumer hook: skill trees live under this root, one dir per
        // mount name.
        env.insert("DUCKTAPE_RUN_SKILLS".into(), ro.display().to_string());
    }
    if let Some(url) = node_url {
        env.insert("DUCKTAPE_NODE".into(), url.to_string());
    }
    if let Some(agent) = &spec.agent {
        env.insert("DUCKTAPE_RUN_AGENT".into(), agent.agent_id.clone());
        env.insert("DUCKTAPE_RUN_ID".into(), agent.run_id.clone());
        // the AUTHOR of anything the run commits is the agent, on every lane:
        // a forge checkout's commits and a push the run makes itself from a
        // duckfs workspace attribute the same way.
        env.insert(
            "GIT_AUTHOR_NAME".into(),
            forge::sanitize_display_name(&agent.display_name),
        );
        env.insert(
            "GIT_AUTHOR_EMAIL".into(),
            forge::agent_email(&agent.agent_id),
        );
    }
    if let Some(session) = session {
        env.insert(session::ENV_ACTION_URL.into(), session.action_url.clone());
        env.insert(
            session::ENV_ACTION_TOKEN.into(),
            session.action_token.clone(),
        );
    }
    env
}

/// this node's operator credential as a run's node lane lends it: a fresh
/// read of `admin.token` per use, because the node re-mints it every boot.
fn operator_credential(node: &NodeLink) -> OperatorCredential {
    let node = node.clone();
    OperatorCredential::new(crate::admin::ADMIN_TOKEN_HEADER, move || {
        node.operator_token()
    })
}

/// the file every skill document is read from, inside its mount — the
/// convention this repo's own `skills/` already follow.
const SKILL_DOC: &str = "SKILL.md";

/// materialize the run's W6 skill mounts under `ro_root` — one duckfs checkout
/// per mount at `<ro_root>/<name>` — and ASSEMBLE THE RUN'S SOUL from them. the
/// ONE implementation both lanes call.
///
/// the two jobs live together because this is the only place that has both
/// halves: the committed curation (`mounts`: names, order, load modes) and the
/// materialized bodies. compute-service owns the pure assembly
/// ([`compute_service::assemble_context_doc`]) but cannot cross the reachability
/// wall to read a duckfs checkout; the binary can read files but must not decide
/// the document's shape. so: read here, assemble there.
///
/// the ro root is a SUFFIXED SIBLING of the run's writable tree (`<slug>-ro`),
/// never a child of it: the duckfs lane's `commit` scans only under the rw dir
/// and the forge lane's `git add -A` only sees its own worktree, so a skill
/// tree beside them can never leak into an output snapshot or ride a pushed
/// branch. mount names arrive PRE-VALIDATED ([`mount_dir_name`] + dedup) from
/// [`NodedProvisioner::provision`].
///
/// SYNC on purpose: the engine is blocking std::fs + `block_on` of the actor,
/// so every caller runs this on `spawn_blocking` — NEVER an async worker.
///
/// W5 on the error path: a checkout can fail PARTWAY (transport mid-read,
/// verify mismatch) after materializing some of the tree, and a failed
/// provision hands the run no workspace to clean up — so this removes its OWN
/// debris (the whole ro root) before returning. the caller unwinds the rw tree
/// it already materialized. an over-budget soul (a blown context bound) fails on
/// that same path: the mounts are already on disk when the assembler refuses.
fn checkout_ro_mounts(
    node: &NodeLink,
    ro_root: &Path,
    mounts: &[RoMount],
) -> Result<String, String> {
    // built HERE, inside the caller's blocking context — see `NodeLink::files`.
    let api = node.files();
    mounts
        .iter()
        .map(|m| {
            checkout_with(
                &api,
                &ro_root.join(&m.mount_subpath),
                &m.source_prefix,
                m.source_snapshot.as_deref(),
                &CheckoutOptions::default(),
            )
            .map_err(|e| e.to_string())?;
            read_skill_doc(ro_root, m)
        })
        .collect::<Result<Vec<_>, _>>()
        .and_then(|docs| assemble_context_doc(&docs))
        .inspect_err(|_| {
            let _ = std::fs::remove_dir_all(ro_root);
        })
}

/// read one materialized mount's `SKILL.md` into the plain-data [`SkillDoc`] the
/// assembler takes. the mount's COMMITTED name is the heading — never a name
/// read out of the document, which would let a doc rename itself into another
/// agent's persona.
///
/// the two degrade rules are deliberately asymmetric:
/// - an `always` skill whose body is missing or unreadable FAILS THE RUN. it is
///   the agent's persona: running without it would quietly produce a different
///   agent, which is exactly the class of silent corruption a loud failure is
///   cheaper than.
/// - an on-demand skill degrades to a name-only index entry (no body read is
///   even needed for it beyond its description) — a cosmetic parse must never
///   cost a run. non-utf8 or frontmatter-less is the same story: index it.
fn read_skill_doc(ro_root: &Path, mount: &RoMount) -> Result<SkillDoc, String> {
    let path = ro_root.join(&mount.mount_subpath).join(SKILL_DOC);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if mount.always => {
            return Err(format!(
                "skill {:?} is loaded ALWAYS (it is this agent's persona) but its \
                 {SKILL_DOC} could not be read at {}: {e} — refusing to run a \
                 different agent",
                mount.mount_subpath,
                path.display()
            ));
        }
        // on-demand: the agent is told the skill exists and where to read it;
        // whether THIS node could read the body now changes nothing about the
        // run it is about to do.
        Err(_) => None,
    };
    let (description, body) = text.as_deref().map(parse_skill_md).unwrap_or_default();
    Ok(SkillDoc {
        name: mount.mount_subpath.clone(),
        description,
        body,
        always: mount.always,
    })
}

/// the real provisioner: mints per-run workspaces under `root`, driving the
/// duckfs engine over the node's `/v1` lane and (when [`Self::with_forge`]
/// configured a usable lane) the forge worktree engine over host `git`.
pub struct NodedProvisioner {
    node: NodeLink,
    root: PathBuf,
    /// the forge lane: `Ok` when this node can provision forge worktrees,
    /// `Err(reason)` — decided ONCE at construction, permanent and loud —
    /// when it can't. the duckfs lane is unaffected either way.
    forge: Result<forge::ForgeLane, String>,
    /// this node's http base, handed to every run as `DUCKTAPE_NODE` so its
    /// tool plane can dial back. `None` = no http surface = the var is unset
    /// (see [`Self::with_node_url`]).
    node_url: Option<String>,
}

impl NodedProvisioner {
    pub fn new(node: NodeLink, root: impl Into<PathBuf>) -> Self {
        Self {
            node,
            root: root.into(),
            forge: Err("this provisioner was built without a forge lane \
                        (with_forge was never called)"
                .into()),
            node_url: None,
        }
    }

    /// bind the agent tool plane to this node: `node_url` is the base every run
    /// dials as `DUCKTAPE_NODE` ([`node_http_base`] derives it from the node's
    /// http listen address). `None` — a node serving no http surface — leaves
    /// the var unset: there is nothing for a run's tools to talk to, and a
    /// guessed URL would be worse than an absent one.
    pub fn with_node_url(mut self, node_url: Option<String>) -> Self {
        self.node_url = node_url;
        self
    }

    /// Configure local Git materialization and node-authorized module publication.
    /// Git is probed once before any workspace can be provisioned.
    pub fn with_forge(self, committer_name: impl Into<String>) -> Self {
        self.with_forge_probed(committer_name, forge::probe_host_git)
    }

    /// [`Self::with_forge`] with the construction-time probe injected — the
    /// seam that lets tests exercise a probe failure without uninstalling git.
    fn with_forge_probed(
        mut self,
        committer_name: impl Into<String>,
        probe: impl FnOnce() -> Result<(), String>,
    ) -> Self {
        self.forge = forge::ForgeLane::configure(&self.node, committer_name.into(), probe);
        if let Err(reason) = &self.forge {
            tracing::warn!(
                target: "ducktape::saga",
                error = %reason,
                "forge workspace provisioning unavailable on this node"
            );
        }
        self
    }
}

#[async_trait::async_trait]
impl WorkspaceProvisioner for NodedProvisioner {
    async fn provision(
        &self,
        spec: &WorkspaceSpec,
    ) -> Result<Box<dyn ProvisionedWorkspace>, String> {
        let slug = run_slug(&spec.run_id);
        let run_dir = self.root.join(&slug);
        // validate every skill mount name BEFORE dispatching to a lane: a bad
        // name fails the provision with ZERO debris on disk, whichever lane
        // would have materialized. duplicates are refused too — two mounts
        // sharing a name would silently merge into one checkout dir.
        let mut names = std::collections::HashSet::new();
        for m in &spec.ro_mounts {
            mount_dir_name(&m.mount_subpath)?;
            if !names.insert(m.mount_subpath.as_str()) {
                return Err(format!(
                    "duplicate skill mount subpath {:?}",
                    m.mount_subpath
                ));
            }
        }
        let ro_root = self.root.join(format!("{slug}-ro"));
        match &spec.source {
            WorkspaceSource::Duckfs {
                source_prefix,
                source_snapshot,
            } => {
                duckfs::provision(
                    self.node.clone(),
                    run_dir,
                    ro_root,
                    source_prefix.clone(),
                    source_snapshot.clone(),
                    self.node_url.clone(),
                    spec,
                )
                .await
            }
            WorkspaceSource::Forge { repo, .. } => match &self.forge {
                Ok(lane) => {
                    forge::provision(
                        lane,
                        self.node.clone(),
                        run_dir,
                        ro_root,
                        self.node_url.clone(),
                        spec,
                    )
                    .await
                }
                // a loud attempt failure BEFORE any on-disk debris — the saga
                // settles the attempt (liveness is its job, not ours).
                Err(reason) => Err(format!(
                    "forge workspace provisioning for repo {repo:?} is unavailable on this \
                     node: {reason}"
                )),
            },
        }
    }
}

async fn cleanup_dirs(dir: PathBuf, ro_dir: Option<PathBuf>) {
    let _ = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_dir_all(dir);
        if let Some(ro) = ro_dir {
            let _ = std::fs::remove_dir_all(ro);
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_root_is_salted_per_storage_and_refuses_a_root_inside_storage() {
        // no env mutation (parallel tests share the process): exercise the
        // testable core directly.
        let scratch = std::env::temp_dir().join("ducktape-runs-root-test");
        let storage_a = scratch.join("storage-a");
        let storage_b = scratch.join("storage-b");
        std::fs::create_dir_all(&storage_a).unwrap();
        std::fs::create_dir_all(&storage_b).unwrap();
        let base = scratch.join("runs-base");

        // co-located nodes (distinct storage) get DISJOINT roots — one node's
        // W5 cleanup can never touch a sibling's in-flight checkout.
        let a = runs_root_under(base.clone(), &storage_a).unwrap();
        let b = runs_root_under(base.clone(), &storage_b).unwrap();
        assert_ne!(a, b, "the storage-path salt separates co-located nodes");
        assert!(a.starts_with(&base) && b.starts_with(&base));
        // deterministic per storage: a restart reuses the same root.
        assert_eq!(a, runs_root_under(base.clone(), &storage_a).unwrap());

        // D7 is ENFORCED, not advisory: a base inside <storage> is refused.
        let err = runs_root_under(storage_a.join("agent-runs"), &storage_a).unwrap_err();
        assert!(
            err.contains("D7 forbids") && err.contains("DUCKTAPE_AGENT_RUNS_ROOT"),
            "the refusal names the invariant and the remedy: {err}"
        );
    }

    #[test]
    fn run_slug_is_bounded_alnum_and_collision_free_per_attempt() {
        // pure [a-z0-9], bounded, never empty, no traversal metacharacter survives.
        for id in ["s1:0", "../../etc/passwd", "", "A/B.C-D", &"z".repeat(200)] {
            let s = run_slug(id);
            assert!(
                !s.is_empty() && s.len() <= 48,
                "slug {s:?} bounded+non-empty"
            );
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
                "slug {s:?} is [a-z0-9] — no path-traversal metacharacter"
            );
        }
        // THE bug this guards: distinct attempts of one saga (id differs only in
        // the ":{attempt}" tail) must map to DISTINCT dirs so overlapping
        // attempts never corrupt one checkout.
        let a0 = run_slug("dispatch\u{1f}r\u{1f}deadbeefdeadbeef:0");
        let a1 = run_slug("dispatch\u{1f}r\u{1f}deadbeefdeadbeef:1");
        assert_ne!(a0, a1, "attempt 0 and 1 get distinct dirs");
        // deterministic per run_id (idempotent provision + cleanup).
        assert_eq!(run_slug("saga:2"), run_slug("saga:2"));
    }

    /// a materialized mount on disk — what `checkout_ro_mounts` leaves behind.
    fn materialize(ro_root: &Path, name: &str, body: Option<&str>) -> RoMount {
        std::fs::create_dir_all(ro_root.join(name)).unwrap();
        if let Some(body) = body {
            std::fs::write(ro_root.join(name).join(SKILL_DOC), body).unwrap();
        }
        RoMount {
            source_prefix: format!("/shared/skills/{name}"),
            source_snapshot: None,
            mount_subpath: name.into(),
            always: false,
        }
    }

    #[test]
    fn an_always_skill_with_no_readable_body_fails_the_run_loudly() {
        // THE asymmetry: an `always` skill IS the agent's persona. a run that
        // silently proceeds without it is a DIFFERENT agent answering under the
        // same name — so the missing body is a loud provision failure, never a
        // degrade. (an on-demand body is another matter: see below.)
        let ro_root = std::env::temp_dir().join(format!("soul-always-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ro_root);
        let mut mount = materialize(&ro_root, "persona", None);
        mount.always = true;

        let err = read_skill_doc(&ro_root, &mount).unwrap_err();
        assert!(err.contains("persona"), "names the skill: {err}");
        assert!(
            err.contains("refusing to run a different agent"),
            "states WHY it is fatal: {err}"
        );
        let _ = std::fs::remove_dir_all(&ro_root);
    }

    #[test]
    fn an_on_demand_skill_degrades_to_a_name_only_entry_rather_than_failing() {
        // a cosmetic parse (or an unreadable body) on a skill the agent was only
        // ever going to READ IF RELEVANT must never cost the run: it degrades to
        // a name-only index entry, and the agent can still go look.
        let ro_root = std::env::temp_dir().join(format!("soul-ondemand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ro_root);

        let missing = materialize(&ro_root, "gone", None);
        let doc = read_skill_doc(&ro_root, &missing).expect("an on-demand miss never fails a run");
        assert_eq!(doc.name, "gone");
        assert_eq!(doc.description, None);
        assert!(doc.body.is_empty());

        // frontmatter present ⇒ the description reaches the index, and the
        // heading is the COMMITTED mount name, never a name from the document.
        let described = materialize(
            &ro_root,
            "release",
            Some("---\nname: not-my-name\ndescription: Cut a release.\n---\nthe body\n"),
        );
        let doc = read_skill_doc(&ro_root, &described).unwrap();
        assert_eq!(doc.name, "release", "a doc cannot rename itself");
        assert_eq!(doc.description.as_deref(), Some("Cut a release."));
        assert_eq!(doc.body, "the body\n");
        let _ = std::fs::remove_dir_all(&ro_root);
    }

    #[test]
    fn mount_dir_name_refuses_traversal_and_admits_plain_names() {
        // the trust boundary for consensus-supplied skill names: `provision`
        // runs this over EVERY mount before materializing anything, so a
        // rejection here is a loud provision failure with zero disk debris.
        for bad in [
            "..",
            ".",
            "",
            "../escape",
            "a/b",
            "a\\b",
            "/abs",
            "name with space",
            "name\0nul",
        ] {
            assert!(mount_dir_name(bad).is_err(), "{bad:?} must be refused");
        }
        for good in ["skill", "my-skill_v2", "..dots.ok..", "A.B"] {
            assert!(mount_dir_name(good).is_ok(), "{good:?} must be admitted");
        }
    }

    #[test]
    fn mount_dir_name_agrees_with_the_consensus_side_predicate() {
        // `runs::validate_skills` and this provisioner gate the SAME name
        // shape through ONE function (`runs::is_skill_mount_name`) — this
        // pins that `mount_dir_name` is nothing but a call into it, so the
        // two can never drift apart again.
        for name in [
            "..",
            ".",
            "",
            "../escape",
            "a/b",
            "a\\b",
            "/abs",
            "name with space",
            "name\0nul",
            "skill",
            "my-skill_v2",
            "..dots.ok..",
            "A.B",
            "code review",
            "qa",
        ] {
            assert_eq!(
                mount_dir_name(name).is_ok(),
                is_skill_mount_name(name),
                "{name:?}: mount_dir_name and is_skill_mount_name disagree"
            );
        }
    }

    #[test]
    fn the_consensus_run_id_rides_every_provisioned_run_session_or_not() {
        // the tool plane names the run to the model BY THIS ID, and a run whose
        // session was never opened must still be named — so the id is exported
        // independently of the write half. it is identity, not a credential:
        // nothing but the id crosses in the env.
        let spec = WorkspaceSpec {
            run_id: "s1:0".into(),
            agent: Some(compute_service::AgentExecution {
                native_conversation: None,
                run_id: "chat\u{1f}general\u{1f}2\u{1f}bot".into(),
                attempt: 0,
                agent_id: "bot".into(),
                display_name: "Bot".into(),
            }),
            source: WorkspaceSource::Duckfs {
                source_prefix: "/shared/agent-workspaces/bot".into(),
                source_snapshot: None,
            },
            ro_mounts: Vec::new(),
        };
        let env = run_env(Path::new("/tmp/ws"), None, None, &spec, None);
        assert_eq!(
            env.get("DUCKTAPE_RUN_ID").map(String::as_str),
            spec.agent.as_ref().map(|agent| agent.run_id.as_str())
        );
        // the write half is absent without a session.
        assert!(!env.contains_key(session::ENV_ACTION_URL));
        assert!(!env.contains_key(session::ENV_ACTION_TOKEN));

        // a receipt-only spec names no run, so it exports none.
        let receipt = WorkspaceSpec {
            agent: None,
            ..spec
        };
        assert!(
            !run_env(Path::new("/tmp/ws"), None, None, &receipt, None)
                .contains_key("DUCKTAPE_RUN_ID")
        );
    }
}
