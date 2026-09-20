//! who this run is.
//!
//! the environment carries which node and agent, plus a narrow host endpoint
//! for actions made by this run. The session private key never enters the child.
//! it carries NOTHING about the model's record: that is read back from the
//! committed Runs model configuration, so this module reports what consensus
//! actually holds.
//!
//! ## writes are validated in CONSENSUS, not here
//!
//! this binary does not decide whether a write is well-formed. it asks the
//! scoped host endpoint to sign a runs message; the runs module then checks —
//! on every validator — that the origin IS the session key bound to that run
//! and that the run is still in flight, and the target module decides whether
//! the message it carries is one it accepts. a refusal comes back as the
//! module's own words.
//!
//! a frame's origin is its verified public key. The endpoint accepts only
//! `RunsMsg::AgentAction` for its exact run id, so its bearer token is not a
//! general-purpose signer even if the child reads its environment.
//!
//! READS are not gated: `/v1/query` is ambient to any local process anyway,
//! and a run reads what any member reads.

use std::time::Duration;

use crate::module_contracts::runs::{self, ModelRecord};
use serde_json::json;

use crate::node::{Node, NodeError, Result};

pub const ENV_NODE: &str = "DUCKTAPE_NODE";
pub const ENV_AGENT: &str = "DUCKTAPE_RUN_AGENT";
pub const ENV_WORKSPACE: &str = "DUCKTAPE_RUN_WORKSPACE";
pub const ENV_SKILLS: &str = "DUCKTAPE_RUN_SKILLS";
pub const ENV_ACTION_URL: &str = "DUCKTAPE_RUN_ACTION_URL";
pub const ENV_ACTION_TOKEN: &str = "DUCKTAPE_RUN_ACTION_TOKEN";
/// the run this session is bound to — the `run_id` every action names.
pub const ENV_RUN_ID: &str = "DUCKTAPE_RUN_ID";
const ENV_PROVIDER_CONTROL_URL: &str = "DUCKTAPE_PROVIDER_CONTROL_URL";
const ENV_PROVIDER_CONTROL_TOKEN: &str = "DUCKTAPE_PROVIDER_CONTROL_TOKEN";
const PROVIDER_CONTROL_HEADER: &str = "x-ducktape-provider-control";

/// Model configuration and run requests belong to the runs module.
pub const TARGET_MODEL: &str = "runs";
/// Scoped action proposals go to runs; the user program performs their writes.
pub const TARGET_RUNS: &str = "runs";

/// The narrow host signer endpoint for this live run. Its random token can ask
/// for Runs agent actions/calls only; no general-purpose private key crosses
/// into this process.
struct ActionControl {
    client: reqwest::blocking::Client,
    url: String,
    token: String,
}

/// this run, as the tool plane sees it.
pub struct Run {
    pub node: Node,
    /// `None` when `DUCKTAPE_RUN_AGENT` is unset — this server was started
    /// outside a provisioned run. reads still work; `whoami` says so.
    pub agent_id: Option<String>,
    pub workspace: Option<String>,
    pub skills: Option<String>,
    /// the CONSENSUS run id this server is bound to, from `DUCKTAPE_RUN_ID`.
    /// exported for every provisioned run, session or not: it is identity, not
    /// a credential.
    run_id: Option<String>,
    /// Absent when this MCP server starts without a scoped action endpoint.
    /// Writes then refuse: only a provisioned run has the credential to act
    /// under its program account.
    action: Option<ActionControl>,
    provider_control: Option<ProviderControl>,
}

impl Run {
    /// read the run out of the PROCESS environment — how `ducktape mcp` sees
    /// itself.
    pub fn from_env() -> Self {
        Self::from_vars(&|key| std::env::var(key).ok())
    }

    /// read the run out of an explicit environment — how the compute daemon's
    /// per-run http host sees it.
    ///
    /// `var` answers from the run's OWN host-side env, built before any of it
    /// was rewritten for the guest, so the node url and the action endpoint are
    /// the host's real ones. The guest names none of this: it reaches the tool
    /// plane over its run's lane and nothing else, so it can neither choose the
    /// agent it writes as nor the endpoint it writes through.
    ///
    /// never fails: a missing variable degrades the affected tools, it does not
    /// stop the server. a runner whose MCP server dies at launch is a worse
    /// failure than one whose tools say, in words the model can read, that they
    /// are unbound.
    pub fn from_vars(var: &dyn Fn(&str) -> Option<String>) -> Self {
        let present = |key: &str| var(key).filter(|value| !value.is_empty());
        Self {
            node: Node::new(present(ENV_NODE)),
            agent_id: present(ENV_AGENT),
            workspace: present(ENV_WORKSPACE),
            skills: present(ENV_SKILLS),
            run_id: present(ENV_RUN_ID),
            action: ActionControl::from_vars(&present),
            provider_control: ProviderControl::from_vars(&present),
        }
    }

    /// the run this MCP session is bound to. The signer stays private; callers
    /// that only need an evidence id never get access to the session key.
    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    /// Propose one catalog action mid-run through this run's scoped host
    /// signer, under the caller's idempotency key, and return its committed
    /// receipt.
    ///
    /// there is NO decoding of the envelope here. the runs module decodes it,
    /// on every validator, against the catalog it owns — and its refusal, or
    /// the target module's, is what comes back. a second decoder in this
    /// process could only ever drift from the one that actually decides.
    pub fn act(
        &self,
        request_id: String,
        action: runs::ActionEnvelope,
    ) -> Result<serde_json::Value> {
        self.action
            .as_ref()
            .ok_or_else(|| {
                NodeError::Rejected(format!(
                    "this run has no scoped action endpoint ({ENV_ACTION_URL} is unset), so writing is refused"
                ))
            })?
            .submit(runs::RunsMsg::AgentAction {
                run_id: self.run_id().unwrap_or_default().to_string(),
                request_id,
                action,
            })
    }

    /// Ask the host-local controller for more silent provider time. The model
    /// supplies no identity or credential: both arrive as ambient run env and
    /// the broker rotates them for every child invocation.
    pub fn extend_provider_idle(
        &self,
        request_id: String,
        requested_secs: u64,
    ) -> Result<serde_json::Value> {
        let Some(control) = &self.provider_control else {
            return Ok(json!({"status":"denied", "reason":"unavailable"}));
        };
        control.request(request_id, requested_secs)
    }

    /// the agent's COMMITTED record, or `None` when this server acts for no
    /// agent (`DUCKTAPE_RUN_AGENT` unset). having no identity is an answer,
    /// not a failure: it is a read, and reads are not gated.
    ///
    /// fetched per call rather than cached at startup: an owner can pause or
    /// reconfigure an agent mid-run, and a cached record would keep reporting
    /// what consensus has already changed.
    pub fn record(&self) -> Result<Option<ModelRecord>> {
        let Some(agent_id) = self.agent_id.as_deref() else {
            return Ok(None);
        };
        let reply = self.node.query(
            TARGET_MODEL,
            json!({"model": {"query": {"agent": {"agent_id": agent_id}}}}),
        )?;
        // ModelReply::Agent(Option<ModelRecord>) — snake_case externally
        // tagged, so the record sits under "agent" and is null for an id the
        // registry does not hold.
        let record = reply
            .get("model")
            .and_then(|model| model.get("agent"))
            .ok_or_else(|| {
                NodeError::Transport(format!(
                    "the Runs model query answered a shape this server does not understand: {reply}"
                ))
            })?;
        if record.is_null() {
            return Err(NodeError::Rejected(format!(
                "Runs holds no model {agent_id:?}"
            )));
        }
        serde_json::from_value(record.clone())
            .map(Some)
            .map_err(|e| NodeError::Transport(format!("the Runs model record did not decode: {e}")))
    }
}

const ACTION_HEADER: &str = "x-ducktape-run-action";

impl ActionControl {
    fn from_vars(var: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let url = var(ENV_ACTION_URL).filter(|value| action_url_allowed(value))?;
        let token = var(ENV_ACTION_TOKEN).filter(|value| provider_control_token_allowed(value))?;
        // the signer is scoped to ONE run and every message it signs names it,
        // so a session with no run id can sign nothing.
        var(ENV_RUN_ID)?;
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(60))
            .build()
            .expect("a loopback action client always builds");
        Some(Self { client, url, token })
    }

    fn submit(&self, message: runs::RunsMsg) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(&self.url)
            .header(ACTION_HEADER, &self.token)
            .json(&json!({"message": message}))
            .send()
            .map_err(|error| NodeError::Transport(error.to_string()))?;
        let status = response.status();
        let value: serde_json::Value = response.json().map_err(|error| {
            NodeError::Transport(format!("action signer returned invalid json: {error}"))
        })?;
        if status.is_success() {
            Ok(value)
        } else {
            Err(NodeError::Rejected(
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("scoped action signer rejected the request")
                    .to_string(),
            ))
        }
    }
}

fn action_url_allowed(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1"))
        && url.port().is_some()
        && url.path() == "/v1/run-action"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

struct ProviderControl {
    client: reqwest::blocking::Client,
    url: String,
    token: String,
}

impl ProviderControl {
    fn from_vars(var: &dyn Fn(&str) -> Option<String>) -> Option<Self> {
        let url = var(ENV_PROVIDER_CONTROL_URL)?;
        let token = var(ENV_PROVIDER_CONTROL_TOKEN)?;
        if !provider_control_url_allowed(&url) || !provider_control_token_allowed(&token) {
            return None;
        }
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("a loopback-only provider control client always builds");
        Some(Self { client, url, token })
    }

    fn request(&self, request_id: String, requested_secs: u64) -> Result<serde_json::Value> {
        let response = match self
            .client
            .post(&self.url)
            .header(PROVIDER_CONTROL_HEADER, &self.token)
            .json(&json!({
                "request_id": request_id,
                "requested_secs": requested_secs,
            }))
            .send()
        {
            Ok(response) => response,
            Err(_) => {
                return Ok(json!({
                    "status":"denied",
                    "reason":"control_unreachable",
                }));
            }
        };
        response.json().map_err(|error| {
            NodeError::Transport(format!(
                "provider idle controller returned an invalid reply: {error}"
            ))
        })
    }
}

fn provider_control_url_allowed(value: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1"))
        && url.port().is_some()
        && url.path() == "/v1/control/provider-idle"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn provider_control_token_allowed(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod provider_control_tests {
    use super::*;

    #[test]
    fn control_endpoint_and_token_are_strictly_host_local() {
        assert!(provider_control_url_allowed(
            "http://127.0.0.1:41043/v1/control/provider-idle"
        ));
        for rejected in [
            "https://127.0.0.1:41043/v1/control/provider-idle",
            "http://localhost:41043/v1/control/provider-idle",
            "http://ducktape-host:41043/v1/control/provider-idle",
            "http://host.containers.internal:41043/v1/control/provider-idle",
            "http://example.com:41043/v1/control/provider-idle",
            "http://127.0.0.1:41043/v1/control/provider-idle?token=leak",
            "http://127.0.0.1:41043/other",
        ] {
            assert!(
                !provider_control_url_allowed(rejected),
                "accepted {rejected}"
            );
        }
        assert!(provider_control_token_allowed(&"a5".repeat(32)));
        assert!(!provider_control_token_allowed(&"A5".repeat(32)));
        assert!(!provider_control_token_allowed("short"));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// a node that answers `/v1/query` from a canned table. one thread, `n`
    /// requests, no framework.
    pub(crate) fn fake_node(replies: Vec<serde_json::Value>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                // drain enough of the request to unblock the client, then answer.
                let mut buf = [0u8; 4096];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let body = reply.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    pub(crate) fn standing_record() -> ModelRecord {
        runs::ModelRecord {
            account: 2,
            agent_id: "worker".into(),
            owner: runs::RunOrigin::External(vec![9; 32]),
            display_name: "Worker".into(),
            capability: "model-1".into(),
            status: runs::ModelStatus::Active,
            role: runs::ModelRole::General,
            created_at: 0,
            updated_at: 0,
            recipe_hash: Vec::new(),
            skills: Vec::new(),
        }
    }

    fn bound_run(node: String, replies: Vec<serde_json::Value>) -> Run {
        Run {
            node: Node::new(Some(fake_node(replies))),
            agent_id: Some("worker".into()),
            workspace: None,
            skills: None,
            run_id: Some(node),
            action: None,
            provider_control: None,
        }
    }

    #[test]
    fn the_record_is_the_committed_standing_record_and_a_missing_one_refuses() {
        let run = bound_run(
            "run-1".into(),
            vec![json!({"model": {"agent": standing_record()}})],
        );
        assert_eq!(
            run.record().expect("the registry answers"),
            Some(standing_record())
        );

        let gone = bound_run("run-1".into(), vec![json!({"model": {"agent": null}})]);
        assert!(matches!(gone.record(), Err(NodeError::Rejected(_))));

        // no agent identity is an answer, not a refusal — and it asks no node.
        let unbound = Run::from_vars(&|_| None);
        assert_eq!(unbound.record().expect("an unbound read answers"), None);
    }

    #[test]
    fn action_endpoint_is_strictly_host_local_and_path_scoped() {
        assert!(action_url_allowed("http://127.0.0.1:41043/v1/run-action"));
        for rejected in [
            "https://127.0.0.1:41043/v1/run-action",
            "http://localhost:41043/v1/run-action",
            "http://ducktape-host:41043/v1/run-action",
            "http://host.containers.internal:41043/v1/run-action",
            "http://127.0.0.1:41043/v1/submit/frame",
            "http://127.0.0.1:41043/v1/run-action?token=leak",
        ] {
            assert!(!action_url_allowed(rejected), "accepted {rejected}");
        }
    }
}
