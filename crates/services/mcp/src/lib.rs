//! the agent tool plane: the MCP server a Ducktape run's CLI talks to.
//!
//! it gives a run access to the network it is running inside: read any module,
//! and write to any module as the run's own program account.
//!
//! ## one catalog, two hosts
//!
//! [`handle`] is the whole server — one JSON-RPC frame in, at most one out, no
//! transport of its own. Two hosts carry it:
//!
//! * the compute daemon's per-run node lane serves it as a streamable-HTTP MCP
//!   endpoint on the ONE tunnel the guest already has
//!   (`provider_host::read_lane`). The tools then run on the HOST, so nothing
//!   about them enters the guest — no binary, no credential, and no way for
//!   the two to be different builds.
//! * `ducktape mcp` serves it over stdin/stdout for a local operator and for
//!   the node's own e2e suites.
//!
//! Both call this one function, because a second implementation of the tool
//! catalog is a tool plane that drifts.
//!
//! ## how it knows who it is
//!
//! [`Run`] carries which node and which agent, plus the run-scoped write
//! endpoint. The HTTP host reads those out of the run's own host-side
//! environment, which is why the guest never supplies them — a header naming
//! the agent would be the guest choosing whose account it writes as. The stdio
//! host reads them out of the process environment. The agent's record itself
//! never travels either way: it is read back from the committed registry, so
//! what the model sees is what consensus actually holds. see [`identity`].
//!
//! ## failure posture
//!
//! it NEVER refuses to start. a missing node, a missing agent, an unreachable
//! daemon — each degrades the affected tools with an error the model can read
//! and reason about, because an MCP server that dies at launch takes the whole
//! run's tool plane with it and tells the model nothing about why.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

mod guide;
mod identity;
mod node;
mod rpc;
mod tools;

pub use self::identity::Run;
pub use self::rpc::Response;

use self::rpc::{PROTOCOL_VERSION, Request, SERVER_NAME, tool_failure, tool_result};

/// JSON-RPC's "method not found".
const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC's "parse error".
const PARSE_ERROR: i32 = -32700;

/// serve the tool plane over stdin/stdout, one JSON object per line, until
/// the runner closes stdin.
pub fn serve_stdio() {
    let run = Run::from_env();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            // stdin died under us — the runner is gone, and so is any reason to
            // keep running.
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle(&run, &line) else {
            // a notification. answering one is a protocol violation; silence is
            // the correct reply.
            continue;
        };
        let encoded = serde_json::to_string(&response).expect("a response always serializes");
        // a broken pipe means the runner exited. nothing left to say.
        if writeln!(stdout, "{encoded}").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

/// one frame in, at most one frame out. `None` == the frame was a notification.
pub fn handle(run: &Run, line: &str) -> Option<Response> {
    let request: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        // a frame we cannot even parse has no id to answer against; JSON-RPC
        // says to answer with a null id, so a client waiting on a malformed
        // send is not left hanging.
        Err(e) => {
            return Some(Response::err(
                Value::Null,
                PARSE_ERROR,
                format!("could not parse the request: {e}"),
            ));
        }
    };
    // a notification carries no id and MUST NOT be answered — `initialized` is
    // the one every client sends.
    let id = request.id?;

    Some(match request.method.as_str() {
        "initialize" => Response::ok(id, initialize()),
        "tools/list" => Response::ok(id, tools::list()),
        "tools/call" => Response::ok(id, call(run, &request.params)),
        // `ping` is in the spec and clients do send it; an empty result is the
        // whole contract.
        "ping" => Response::ok(id, json!({})),
        other => Response::err(
            id,
            METHOD_NOT_FOUND,
            format!("this server does not implement {other:?}"),
        ),
    })
}

fn initialize() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools": {}},
        "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
        // where the "how to work in Ducktape" guide lives — it ships with the
        // binary, so it can never describe a tool this binary does not have.
        "instructions": guide::GUIDE,
    })
}

/// dispatch one `tools/call`.
///
/// EVERY outcome here is a `result`, never a JSON-RPC `error` — a tool that
/// refuses (a module rejection, an unreachable endpoint, an unknown tool name)
/// must reach the MODEL as content it can read and adapt to, not the runner as
/// a broken server it should give up on. that is the difference between an
/// agent that says "chat refused that message" and one whose tool plane
/// silently dies.
fn call(run: &identity::Run, params: &Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return tool_failure("a tools/call needs a \"name\"");
    };
    let Some(tool) = tools::find(name) else {
        return tool_failure(format!("{name:?} is not a tool this server offers"));
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match (tool.handler)(run, &args) {
        Ok(value) => tool_result(&value),
        Err(e) => tool_failure(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// a run bound to nothing — the shape this server takes when it is started
    /// outside a provisioned run. every tool must still ANSWER.
    fn unbound() -> identity::Run {
        // from_env with none of the vars set: the test binary's own environment
        // carries no DUCKTAPE_* (and the e2e sets them per-child, not here).
        identity::Run::from_env()
    }

    fn request(method: &str, params: Value) -> String {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string()
    }

    fn result_of(response: &Response) -> Value {
        serde_json::to_value(response).unwrap()["result"].clone()
    }

    #[test]
    fn initialize_states_the_protocol_and_carries_the_guide() {
        let resp = handle(&unbound(), &request("initialize", json!({}))).expect("a request");
        let result = result_of(&resp);
        assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result["capabilities"]["tools"].is_object());
        // the guide is the model's only orientation; an empty one is a bug that
        // would be invisible in production.
        let instructions = result["instructions"].as_str().unwrap();
        assert!(instructions.contains("ducktape_whoami"));
        assert!(instructions.contains("DUCKTAPE_RUN_SKILLS") || instructions.contains("workspace"));
    }

    #[test]
    fn a_notification_is_never_answered() {
        // the `initialized` notification carries no id. answering it is a
        // protocol violation some clients treat as fatal.
        let frame = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string();
        assert!(handle(&unbound(), &frame).is_none());
    }

    #[test]
    fn an_unknown_method_is_a_protocol_error() {
        let resp = handle(&unbound(), &request("tools/nope", json!({}))).expect("a request");
        let encoded = serde_json::to_value(&resp).unwrap();
        assert_eq!(encoded["error"]["code"], METHOD_NOT_FOUND);
        assert!(encoded["result"].is_null());
    }

    #[test]
    fn an_unparseable_frame_answers_against_a_null_id() {
        let resp = handle(&unbound(), "{not json").expect("a request");
        let encoded = serde_json::to_value(&resp).unwrap();
        assert_eq!(encoded["error"]["code"], PARSE_ERROR);
        assert!(encoded["id"].is_null());
    }

    #[test]
    fn tools_list_carries_every_tool_with_a_schema() {
        let resp = handle(&unbound(), &request("tools/list", json!({}))).expect("a request");
        let listed = result_of(&resp);
        let tools = listed["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        for t in tools {
            assert!(t["name"].as_str().unwrap().starts_with("ducktape_"));
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn an_unknown_tool_is_a_tool_failure_not_a_protocol_error() {
        // the distinction that lets a model recover: a bad tool NAME is content
        // it can read, not a transport fault the runner reports as a dead
        // server.
        let resp = handle(
            &unbound(),
            &request("tools/call", json!({"name": "ducktape_nope"})),
        )
        .expect("a request");
        let encoded = serde_json::to_value(&resp).unwrap();
        assert!(encoded["error"].is_null(), "must not be a protocol error");
        assert_eq!(encoded["result"]["isError"], true);
    }

    #[test]
    fn an_unbound_server_still_answers_every_tool_call() {
        // the failure posture: no node, no agent — and every tool still returns
        // a readable refusal rather than killing the run's tool plane. whoami
        // needs neither: with no agent, who you are is nobody, and it answers.
        for tool in tools::all() {
            let answers_unbound = tool.name == "ducktape_whoami";
            let resp = handle(
                &unbound(),
                &request("tools/call", json!({"name": tool.name, "arguments": {}})),
            )
            .expect("a request");
            let encoded = serde_json::to_value(&resp).unwrap();
            assert!(
                encoded["error"].is_null(),
                "{} answered with a protocol error",
                tool.name
            );
            assert_eq!(
                encoded["result"]["isError"], !answers_unbound,
                "{} unbound: {encoded}",
                tool.name
            );
        }
    }
}
