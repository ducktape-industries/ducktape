//! the MCP wire: JSON-RPC 2.0, one object per line, over stdin/stdout.
//!
//! deliberately hand-rolled rather than pulled from a crate. the server speaks
//! exactly four methods (`initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`) and a protocol that is one struct deep — an SDK
//! would be more code to configure than to replace, and it would put a
//! dependency between the agent tool plane and someone else's release cadence.
//!
//! framing rule: a REQUEST carries an `id` and gets exactly one response; a
//! NOTIFICATION carries none and gets NOTHING back. answering a notification is
//! a protocol violation that some clients treat as fatal, so the loop must be
//! able to tell them apart — hence `id: Option<Value>` rather than a defaulted
//! id.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// the protocol revision this server implements. echoed back in `initialize`;
/// a client asking for a different one still gets this — MCP's negotiation is
/// "the server states what it speaks", not a handshake that can fail.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub const SERVER_NAME: &str = "ducktape";

#[derive(Deserialize)]
pub struct Request {
    /// absent on a notification — see the framing rule above.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Serialize)]
pub struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
    code: i32,
    message: String,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// a PROTOCOL error — an unknown method, an unparseable frame. a failing
    /// TOOL is not this: a tool that refuses (a module that said no, a node
    /// that could not be reached) returns a normal `result` carrying
    /// `isError: true`, so the model sees the refusal as content it can react
    /// to rather than as a transport fault it cannot. see [`tool_failure`].
    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// the `tools/call` success shape: MCP wraps every tool answer in a content
/// list. structured payloads ride as pretty-printed json text — the model reads
/// it, and pretty-printing costs nothing next to the model's own token budget.
pub fn tool_result(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({"content": [{"type": "text", "text": text}], "isError": false})
}

/// the `tools/call` failure shape. `isError` — NOT a JSON-RPC error — so the
/// refusal reaches the MODEL as readable content ("task already exists")
/// instead of surfacing to the runner as a broken tool server.
/// that distinction is the whole reason an agent can recover from being denied.
pub fn tool_failure(message: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true,
    })
}
