//! `NodeLink` — a service daemon's handle on the node it serves.
//!
//! The compute plane runs OUT OF PROCESS now (`ducktape service run compute`),
//! so everything it used to reach through the in-process [`crate::NodeHandle`]
//! actor lane travels the node's own `/v1` surface instead. This is the one
//! place that mapping lives:
//!
//! | actor command | `/v1` route |
//! |---|---|
//! | `NodeCommand::Submit` | `POST /v1/submit` — the node re-signs with ITS key |
//! | `NodeCommand::SubmitFrame` | `POST /v1/submit/frame` — verbatim, signer verified |
//! | `NodeCommand::Query` | `POST /v1/query` — committed module state |
//! | `ActorNodeApi` (duckfs engine) | [`duckfs_client::http::HttpNode`] |
//!
//! The `origin` field of `/v1/submit` is deliberately not sent: `bin/node`
//! discards it and frames the op with the node key, which is exactly the
//! identity a saga lease is held under. That equivalence is the whole reason a
//! daemon needs no keypair of its own — and it is exactly why `/v1/submit`
//! takes the OPERATOR credential (`signed_req`'s `Authority::Operator`, #1808)
//! rather than any acting key: `submit` below rides [`Self::post_json`], which
//! attaches it on every call via [`Self::credentialed`].
//!
//! Two things stay host-local paths rather than `/v1` calls, because they are
//! host resources and not node state: the guest images a run boots from, and
//! the forge module's materialized bare repos (`<storage>/forge-repo`), which
//! the worktree lane clones from directly. A daemon that boots this host's VMs
//! is already on this host.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use duckfs_client::http::HttpNode;

/// How long one lane call may take by DEFAULT. A submit rides real consensus,
/// so it gets the same generous ceiling `HttpNode` gives a commit; a lane that
/// hangs forever would wedge a run with no diagnosis. A link that only reads
/// committed state on someone's interactive critical path says so with
/// [`NodeLink::with_timeout`] instead of inheriting this.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

fn http_client(timeout: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .expect("build a reqwest client")
}

/// The node a daemon serves, addressed over its local `/v1` surface.
#[derive(Clone)]
pub struct NodeLink {
    base: String,
    /// the node's forge repo base (`<storage>/forge-repo`) — a host path, not a
    /// route. `None` on a node whose storage dir is unknown to the daemon.
    forge_repo: Option<PathBuf>,
    /// the node's workspace — the PATH the operator credential lives at, never
    /// the credential itself. EVERY mutating `/v1` route refuses a caller that
    /// presents neither it nor a user signature ([`crate::signed_req`]), and a
    /// daemon has no user key — its writes ARE the node's. Re-read per attach,
    /// never latched: the node mints a fresh `admin.token` on every boot, so a
    /// daemon that outlives a node restart would otherwise present a dead
    /// credential forever. `None` = reads only.
    workspace: Option<PathBuf>,
    /// both credential warnings are latched: an attach runs per REQUEST, and a
    /// heartbeat that fires every block would turn either into a log bomb.
    /// Shared across clones, so one link warns once however it is cloned.
    warned_unreadable: Arc<AtomicBool>,
    warned_rejected: Arc<AtomicBool>,
    client: reqwest::Client,
}

/// true exactly once per flag — the latch behind the two credential warnings.
fn first_time(flag: &AtomicBool) -> bool {
    flag.compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

impl NodeLink {
    /// address the node at `base` (e.g. `http://127.0.0.1:8844`).
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            forge_repo: None,
            workspace: None,
            warned_unreadable: Arc::new(AtomicBool::new(false)),
            warned_rejected: Arc::new(AtomicBool::new(false)),
            client: http_client(CALL_TIMEOUT),
        }
    }

    /// carry the operator credential out of the node's `workspace` on every
    /// mutating call. A daemon that cannot read it keeps its READS — the link
    /// still works for `/v1/query`, and a write comes back as the node's own
    /// 401 naming the credential, which beats a daemon that refuses to boot.
    pub fn with_workspace_credential(mut self, workspace: &Path) -> Self {
        self.workspace = Some(workspace.to_path_buf());
        self
    }

    /// Bound this link's calls at `timeout` rather than the consensus-sized
    /// default — for a daemon whose link carries only committed READS on a
    /// caller's critical path (the airlock grant gate: a borrower's session is
    /// blocked on it). Fail-closed in seconds beats a two-minute hang while a
    /// node is wedged or restarting.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.client = http_client(timeout);
        self
    }

    /// point the forge worktree lane at the node's materialized bare repos.
    pub fn with_forge_repo(mut self, base: impl Into<PathBuf>) -> Self {
        self.forge_repo = Some(base.into());
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn forge_repo(&self) -> Option<&Path> {
        self.forge_repo.as_deref()
    }

    /// this node's operator credential AS OF NOW — a fresh read of
    /// `admin.token`, because the node re-mints it every boot. `None` on a
    /// read-only link or an unreadable file (warned once).
    pub fn operator_token(&self) -> Option<String> {
        let workspace = self.workspace.as_deref()?;
        match crate::admin::read_operator_token(workspace) {
            Ok(token) => Some(token),
            Err(error) => {
                if first_time(&self.warned_unreadable) {
                    tracing::warn!(
                        target: "ducktape::service",
                        reason = "operator_token_unreadable",
                        %error,
                        "this daemon cannot write to its node"
                    );
                }
                None
            }
        }
    }

    /// the duckfs checkout/commit engine's transport.
    ///
    /// MUST be called from a blocking context (`spawn_blocking`), never from an
    /// async one: the engine is sync `std::fs`, and `reqwest`'s blocking client
    /// owns a runtime whose DROP panics inside an async context. Building it
    /// where it is used keeps both halves on the blocking side.
    pub fn files(&self) -> HttpNode {
        // the closure re-reads the credential per write, exactly like every
        // other attach: an engine built once at boot must not pin this boot's
        // token into every later commit.
        let link = self.clone();
        HttpNode::new(self.base.clone())
            .with_operator_credential(Arc::new(move || link.operator_token()))
    }

    /// submit one module op. `payload` is the module's own serde_json wire
    /// bytes, which `/v1/submit` takes as a json value and re-serializes — the
    /// module decodes the same value either way.
    pub async fn submit(&self, target: &str, payload: &[u8]) -> Result<u64, String> {
        let payload: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|error| format!("op payload is not json: {error}"))?;
        let body = serde_json::json!({ "target": target, "payload": payload });
        let text = self.post_json("/v1/submit", &body).await?;
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value["height"].as_u64())
            .ok_or_else(|| format!("unexpected submit receipt: {text}"))
    }

    /// Store opaque content under its digest using this service's node authority.
    pub async fn put_blob(&self, bytes: Vec<u8>) -> Result<Vec<u8>, String> {
        use sha2::{Digest as _, Sha256};
        let bounded = bytes.len() <= blobstore::MAX_TRANSFER_BYTES;
        if !bounded {
            return Err("blob exceeds transfer limit".into());
        }
        let digest = Sha256::digest(&bytes).to_vec();
        let response = self
            .credentialed(self.client.post(format!("{}/v1/files/blob", self.base)))
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .map_err(|error| format!("blob upload: {error}"))?;
        let body = Self::body_of(response).await?;
        let reply: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| error.to_string())?;
        let expected = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if reply["digest"] != expected {
            return Err("blob receipt digest mismatch".into());
        }
        Ok(digest)
    }

    /// submit an ALREADY-SIGNED frame verbatim. The node verifies the signature
    /// and the frame's own signer becomes the op's origin, so this is the only
    /// lane that can carry an identity other than the node's.
    pub async fn submit_frame(&self, frame: Vec<u8>) -> Result<(), String> {
        let response = self
            .client
            .post(format!("{}/v1/submit/frame", self.base))
            .header("content-type", "application/octet-stream")
            .body(frame)
            .send()
            .await
            .map_err(|error| format!("POST /v1/submit/frame: {error}"))?;
        Self::body_of(response).await.map(|_| ())
    }

    /// read committed module state. `req` is the module's encoded `*Query`; the
    /// reply is its encoded `*Reply`, so callers decode with the module's own
    /// codec exactly as they did on the actor lane.
    pub async fn query(&self, target: &str, req: &[u8]) -> Result<Vec<u8>, String> {
        let query: serde_json::Value =
            serde_json::from_slice(req).map_err(|error| format!("query is not json: {error}"))?;
        let body = serde_json::json!({ "target": target, "query": query });
        self.post_json("/v1/query", &body)
            .await
            .map(String::into_bytes)
    }

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<String, String> {
        let response = self
            .credentialed(self.client.post(format!("{}{path}", self.base)))
            .json(body)
            .send()
            .await
            .map_err(|error| format!("POST {path}: {error}"))?;
        // a 401 here is the CREDENTIAL, not the request: the node re-mints
        // `admin.token` per boot, so this is what a daemon holding a stale one
        // looks like from the outside. Named, or the caller only ever sees its
        // own generic failure reason.
        let credential_refused = response.status() == reqwest::StatusCode::UNAUTHORIZED;
        if credential_refused && first_time(&self.warned_rejected) {
            tracing::warn!(
                target: "ducktape::service",
                reason = "operator_token_rejected",
                "this node refused the daemon's operator credential"
            );
        }
        Self::body_of(response).await
    }

    /// attach the operator credential when this link has one to read. Harmless
    /// on a read route (the gate never looks) and required on every write.
    fn credentialed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.operator_token() {
            Some(token) => request.header(crate::admin::ADMIN_TOKEN_HEADER, token),
            None => request,
        }
    }

    /// the node's rejection string rides through VERBATIM — the duckfs conflict
    /// taxonomy and the saga's refusal messages both key on the exact text.
    async fn body_of(response: reqwest::Response) -> Result<String, String> {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| value["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            return Err(detail);
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a fake node whose `/v1/query` answers with the operator credential the
    /// caller presented — so a test can assert WHICH token the link attached.
    async fn credential_echo_node() -> String {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind a loopback test surface");
        let address = listener.local_addr().expect("read the test address");
        let app = axum::Router::new().route(
            "/v1/query",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                headers
                    .get(crate::admin::ADMIN_TOKEN_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    async fn presented_credential(link: &NodeLink) -> String {
        let bytes = link.query("t", b"{}").await.expect("query the fake node");
        String::from_utf8(bytes).expect("the echoed credential is utf-8")
    }

    #[tokio::test]
    async fn the_operator_credential_is_re_read_on_every_request() {
        let workspace = tempfile::tempdir().expect("a node workspace");
        let booted = crate::admin::mint_operator_token(workspace.path()).expect("mint");
        let link =
            NodeLink::new(credential_echo_node().await).with_workspace_credential(workspace.path());
        assert_eq!(presented_credential(&link).await, booted);

        // the node restarted and re-minted. A daemon that outlives that restart
        // must present the NEW secret, not the one it read at its own boot.
        let reminted = crate::admin::mint_operator_token(workspace.path()).expect("re-mint");
        assert_ne!(reminted, booted);
        assert_eq!(presented_credential(&link).await, reminted);
    }

    #[tokio::test]
    async fn a_link_without_a_workspace_presents_nothing() {
        let link = NodeLink::new(credential_echo_node().await);
        assert_eq!(link.operator_token(), None);
        assert_eq!(presented_credential(&link).await, "");
    }
}
