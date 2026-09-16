//! the node client: the two read surfaces the tool plane needs, over the http
//! routes `noded` already serves.
//!
//! - `POST /v1/query`  — every module read (chat, tasks, pages, forge, agent).
//! - `GET  /v1/files/*` — the duckfs read verbs, which are their own routes
//!   rather than module queries.
//!
//! Writes use the separately injected run-scoped action endpoint. This client
//! intentionally has no general submit method.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// a failure crossing the node boundary. `Rejected` is the MODULE's own words,
/// passed through verbatim — an agent that submits a squatted id must see the
/// module's actual complaint, not a reworded guess at it.
#[derive(Debug)]
pub enum NodeError {
    /// no `DUCKTAPE_NODE` in the environment: this process was started outside
    /// a provisioned run. every tool call fails with this, cleanly, rather than
    /// the server refusing to start — a runner whose MCP server dies mid-launch
    /// is a far worse failure than one whose tools politely say they are
    /// unbound.
    Unbound,
    /// the module refused the op or the query (a 400 with `{"error": ...}`).
    Rejected(String),
    /// anything else that is not a clean 2xx.
    Transport(String),
}

impl std::fmt::Display for NodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbound => write!(
                f,
                "this Ducktape MCP server is not bound to a node (DUCKTAPE_NODE is unset), so it \
                 can neither read nor write Ducktape state"
            ),
            Self::Rejected(msg) => write!(f, "Ducktape refused the request: {msg}"),
            Self::Transport(msg) => write!(f, "could not reach the Ducktape node: {msg}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, NodeError>;

/// the local node, addressed over http. `base` is `None` when the environment
/// carried no `DUCKTAPE_NODE` — every call then fails [`NodeError::Unbound`].
pub struct Node {
    client: reqwest::blocking::Client,
    base: Option<String>,
}

impl Node {
    pub fn new(base: Option<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            // a loopback daemon is never behind a corporate proxy, and a
            // submit rides real consensus — give it room without hanging a
            // tool call forever.
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("a blocking http client with no tls roots to load always builds");
        Self {
            client,
            base: base.map(|b| b.trim_end_matches('/').to_string()),
        }
    }

    fn base(&self) -> Result<&str> {
        self.base.as_deref().ok_or(NodeError::Unbound)
    }

    /// read a module. `query` is the module's own `*Query` enum as json.
    pub fn query(&self, target: &str, query: Value) -> Result<Value> {
        let url = format!("{}/v1/query", self.base()?);
        let body = json!({"target": target, "query": query});
        self.send(self.client.post(url).json(&body))
    }

    /// read a module's index-tier view (`POST /v1/index/{module}/view`) — the
    /// read model every human/agent-facing list, page, and search comes from.
    /// `query` is the module's own `*ViewQuery` enum as json.
    pub fn view(&self, module: &str, query: Value) -> Result<Value> {
        let url = format!("{}/v1/index/{module}/view", self.base()?);
        self.send(self.client.post(url).json(&query))
    }

    /// one of the duckfs read routes (`ls`, `read`, `grep`, ...), with its
    /// params as the query string.
    pub fn files(&self, verb: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/v1/files/{verb}", self.base()?);
        self.send(self.client.get(url).query(params))
    }

    /// the one place a response becomes a `Result`. a 400 carries the module's
    /// verbatim `{"error": ...}`; everything else non-2xx is transport.
    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let resp = req
            .send()
            .map_err(|e| NodeError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .map_err(|e| NodeError::Transport(e.to_string()))?;
        if status.is_success() {
            return serde_json::from_str(&body)
                .map_err(|e| NodeError::Transport(format!("node reply was not json: {e}")));
        }
        #[derive(Deserialize)]
        struct ErrBody {
            error: String,
        }
        match serde_json::from_str::<ErrBody>(&body) {
            Ok(e) => Err(NodeError::Rejected(e.error)),
            Err(_) => Err(NodeError::Transport(format!("http {status}: {body}"))),
        }
    }
}
