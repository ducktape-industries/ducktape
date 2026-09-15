//! the duckfs lane: materialize / commit / clean a per-run duckfs checkout
//! over the node's `/v1` surface.
//!
//! it runs the exact `checkout_with`/`commit` primitives the
//! `/v1/fs/workspaces` RPC does ([`crate::workspaces`]), on `spawn_blocking`
//! (the engine is sync std::fs + blocking http — NEVER an axum/tokio worker),
//! and drives them through [`NodeLink::files`]. The node's own RPC keeps its
//! in-process `ActorNodeApi` (a daemon dialing its own surface would deadlock
//! the single actor); this runs in the COMPUTE DAEMON, a different process, so
//! the http lane is the direct one, not a self-dial.
//!
//! LIVE, not dormant: both binaries wire the files module unconditionally, so
//! the runs composer emits v1 for
//! every agent run and the pool takes the full provision → bind → run →
//! commit → cleanup bracket through this provisioner. Embedders that do not
//! wire a files module compose the same v1 envelope with a null source pin.

use std::collections::BTreeMap;
use std::path::PathBuf;

use compute_service::{
    ProvisionedWorkspace, WorkspaceReceipt, WorkspaceSource, WorkspaceSpec, assemble_context_doc,
};
use duckfs_client::checkout::{CheckoutOptions, checkout_with};
use duckfs_client::commit::{CommitError, commit};

use crate::node_link::NodeLink;
use provider_host::OperatorCredential;

/// materialize the duckfs source at `dir` (plus W6 skill ro mounts under the
/// sibling `ro_root`) and hand back the live workspace. mount names arrive
/// PRE-VALIDATED by the dispatching provisioner (`mount_dir_name` + dedup).
/// `node_url` is this node's http base (`None` = no http surface), handed to
/// the run as `DUCKTAPE_NODE` so its tool plane can dial back.
#[allow(clippy::too_many_arguments)]
pub(super) async fn provision(
    node: NodeLink,
    dir: PathBuf,
    ro_root: PathBuf,
    prefix: String,
    snapshot: Option<String>,
    node_url: Option<String>,
    spec: &WorkspaceSpec,
) -> Result<Box<dyn ProvisionedWorkspace>, String> {
    let checkout_node = node.clone();
    let checkout_dir = dir.clone();
    // the engine call is blocking std::fs + block_on(actor) — MUST be
    // spawn_blocking (never an async worker), exactly like
    // workspaces.rs::create_workspace. a managed checkout records no node
    // url (its commits ride the actor lane).
    tokio::task::spawn_blocking(move || {
        checkout_with(
            &checkout_node.files(),
            &checkout_dir,
            &prefix,
            snapshot.as_deref(),
            &CheckoutOptions::default(),
        )
        .inspect_err(|_| {
            // a checkout can fail PARTWAY (transport mid-read, verify
            // mismatch) after materializing some of the tree — the run
            // never gets a workspace handle to clean up, so the error
            // path must remove its own debris (W5 applies here too).
            let _ = std::fs::remove_dir_all(&checkout_dir);
        })
    })
    .await
    .map_err(|_| "workspace checkout task panicked".to_string())?
    .map_err(|e| e.to_string())?;
    // the rw checkout is materialized on disk NOW — the one host-observable
    // fact an e2e otherwise has to poll for (the dir lives only seconds and
    // is cleaned up before a filesystem sample could reliably catch it).
    // the message text is the only carrier the harness reads (see
    // `compute_markers`/`materialized_dirs`): a structured `kind`/`path` field
    // here would duplicate the same pair after the fmt layer's coloured field
    // list, corrupting the marker extraction.
    tracing::debug!(
        target: "ducktape::agent",
        "run dir materialized kind=rw path={}", dir.display()
    );
    // W6 skill ro mounts land at a SUFFIXED SIBLING of the rw checkout
    // root (`<slug>-ro/<name>`): `commit` scans only under `dir`, so a
    // skill tree beside it can never leak into the output snapshot. the same
    // step assembles the run's SOUL from those mounts (it is the only place
    // that holds both the curation and the materialized bodies).
    let (ro_dir, context_doc) = if spec.ro_mounts.is_empty() {
        // nothing to mount — but the document still ships. the tool-plane
        // instruction and the library pointer are facts about the world the run
        // wakes up in, not part of the agent's curation: a skill-less agent that
        // is never told the MCP plane exists is a blind one.
        (None, Some(assemble_context_doc(&[])?))
    } else {
        let mount_node = node.clone();
        let mounts = spec.ro_mounts.clone();
        let checkout_ro = ro_root.clone();
        let checkout_rw = dir.clone();
        let context_doc = tokio::task::spawn_blocking(move || {
            super::checkout_ro_mounts(&mount_node, &checkout_ro, &mounts).inspect_err(|_| {
                // W5 again: the run never gets a workspace handle on a
                // failed provision, so the already-materialized rw checkout
                // goes too (the mount helper removed its own partial tree).
                let _ = std::fs::remove_dir_all(&checkout_rw);
            })
        })
        .await
        .map_err(|_| "skill mount checkout task panicked".to_string())??;
        // the ro skill root is the sibling half of the same host-observable
        // fact (see the rw marker above).
        // same reasoning as the rw marker above: message text only.
        tracing::debug!(
            target: "ducktape::agent",
            "run dir materialized kind=ro path={}", ro_root.display()
        );
        (Some(ro_root), Some(context_doc))
    };
    // the workspace EXISTS now, so ask consensus to bind the run's agent session
    // — never before: a bind for a run that failed to materialize would spend an
    // op on a run that never starts.
    let session = match super::session::open(&node, spec, &dir).await {
        Ok(session) => session,
        Err(error) => {
            super::cleanup_dirs(dir.clone(), ro_dir.clone()).await;
            return Err(error);
        }
    };
    let env = super::run_env(
        &dir,
        ro_dir.as_deref(),
        node_url.as_deref(),
        spec,
        session.as_ref(),
    );
    Ok(Box::new(NodedWorkspace {
        node,
        dir,
        ro_dir,
        source: spec.source.clone(),
        env,
        context_doc,
        _session: session,
    }))
}

/// one live materialized workspace: its on-disk dir, the source the receipt
/// echoes, and the node lane its commit rides.
struct NodedWorkspace {
    node: NodeLink,
    dir: PathBuf,
    /// the W6 skill ro root (`<slug>-ro`), `Some` iff the run had mounts —
    /// tracked ONLY so cleanup can remove it; commit never looks at it.
    ro_dir: Option<PathBuf>,
    source: WorkspaceSource,
    env: BTreeMap<String, String>,
    /// the run's assembled soul — its `always` skills inlined, the rest indexed.
    /// `None` when the agent curated no skills. the provider delivers it.
    context_doc: Option<String>,
    /// Owns the scoped signer endpoint for exactly as long as the workspace.
    _session: Option<super::session::RunSession>,
}

impl NodedWorkspace {
    /// a receipt-only spec: `commit`/`no_changes` read only the source coords,
    /// so the run ids/tools/mount are irrelevant here.
    fn receipt_spec(&self) -> WorkspaceSpec {
        WorkspaceSpec {
            run_id: String::new(),
            agent: None,
            source: self.source.clone(),
            ro_mounts: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl ProvisionedWorkspace for NodedWorkspace {
    fn workdir(&self) -> PathBuf {
        self.dir.clone()
    }

    fn env(&self) -> BTreeMap<String, String> {
        self.env.clone()
    }

    fn path_entries(&self) -> Vec<PathBuf> {
        super::tool_path_entries()
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

    async fn commit(
        &self,
        audit_message: &str,
        _proposal: Option<&str>,
    ) -> Result<WorkspaceReceipt, String> {
        let node = self.node.clone();
        let dir = self.dir.clone();
        let message = audit_message.to_string();
        let result = tokio::task::spawn_blocking(move || {
            // Provider HOME/auth/temp/build state is reserved runtime debris,
            // never an agent output facet. Remove it before duckfs scans.
            let _ = std::fs::remove_dir_all(dir.join(provider_host::RUN_RUNTIME_DIR));
            commit(&node.files(), &dir, &message)
        })
        .await
        .map_err(|_| "workspace commit task panicked".to_string())?;
        let spec = self.receipt_spec();
        match result {
            Ok(summary) => Ok(WorkspaceReceipt::committed(
                &spec,
                summary.snapshot,
                summary.height,
                summary.rebased,
            )),
            // the agent wrote nothing — a clean working copy (R2 empty facet).
            Err(CommitError::Nothing) => Ok(WorkspaceReceipt::no_changes(&spec)),
            // a conflict / rejection / transport failure is loud.
            Err(e) => Err(e.to_string()),
        }
    }

    async fn cleanup(&self) {
        super::cleanup_dirs(self.dir.clone(), self.ro_dir.clone()).await;
    }
}
