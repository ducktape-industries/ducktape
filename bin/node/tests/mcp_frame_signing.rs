//! The MCP child gets no frame signer. It can only send a typed Runs message to
//! the host-owned, run-scoped action endpoint provisioned for this invocation.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[path = "../src/wire.rs"]
mod wire;
use wire::runs;

use serde_json::{Value, json};

const RUN_ID: &str = "saga-7:0";
const AGENT_ID: &str = "quackbot";
const TOKEN: &str = "abababababababababababababababababababababababababababababababab";

struct Captured {
    path: String,
    token: Option<String>,
    message: runs::RunsMsg,
}

fn capture_action(tool: &str, arguments: Value) -> (Value, Captured) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind scoped action stub");
    let address = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    // the stub blocks on the connection itself; the receive below bounds the
    // failure, and a stub still waiting then dies with the test process.
    let stub = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept action request");
        let (path, headers, body) = read_request(&mut stream);
        let request: Value = serde_json::from_slice(&body).expect("json request");
        let message = serde_json::from_value(request["message"].clone()).expect("typed RunsMsg");
        tx.send(Captured {
            path,
            token: headers
                .into_iter()
                .find(|(name, _)| name == "x-ducktape-run-action")
                .map(|(_, value)| value),
            message,
        })
        .unwrap();
        write_json(&mut stream, 200, &json!({"message":"ok"}));
    });

    let result = call_tool(
        command(Some(format!("http://{address}/v1/run-action"))),
        tool,
        arguments,
    );
    let captured = rx
        .recv_timeout(Duration::from_secs(15))
        .expect("MCP never called the action endpoint");
    stub.join().expect("action stub");
    (result, captured)
}

fn command(action_url: Option<String>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ducktape"));
    command
        .arg("mcp")
        .env("DUCKTAPE_RUN_AGENT", AGENT_ID)
        .env("DUCKTAPE_RUN_ID", RUN_ID)
        .env_remove("DUCKTAPE_RUN_SESSION_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(url) = action_url {
        command
            .env("DUCKTAPE_RUN_ACTION_URL", url)
            .env("DUCKTAPE_RUN_ACTION_TOKEN", TOKEN);
    } else {
        command
            .env_remove("DUCKTAPE_RUN_ACTION_URL")
            .env_remove("DUCKTAPE_RUN_ACTION_TOKEN");
    }
    command
}

fn call_tool(mut command: Command, tool: &str, arguments: Value) -> Value {
    let mut child = command.spawn().expect("spawn MCP");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    for request in [
        json!({"jsonrpc":"2.0", "id":0, "method":"initialize", "params":{}}),
        json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params":{"name":tool, "arguments":arguments},
        }),
    ] {
        writeln!(stdin, "{request}").expect("write MCP request");
    }
    drop(stdin);
    let responses: Vec<Value> = stdout
        .lines()
        .map(|line| serde_json::from_str(&line.expect("response line")).expect("response json"))
        .collect();
    child.wait().expect("MCP exits");
    responses
        .into_iter()
        .find(|response| response["id"] == 1)
        .expect("tool response")["result"]
        .clone()
}

#[test]
fn a_write_uses_the_exact_run_scoped_endpoint_and_carries_the_envelope_opaque() {
    let (result, captured) = capture_action(
        "ducktape_action",
        json!({
            "operation":"tasks.create",
            "input":{"title":"prove it"},
            "request_id":"prove-it",
        }),
    );
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(captured.path, "/v1/run-action");
    assert_eq!(captured.token.as_deref(), Some(TOKEN));
    match captured.message {
        runs::RunsMsg::AgentAction {
            run_id,
            request_id,
            action,
        } => {
            assert_eq!(run_id, RUN_ID);
            assert_eq!(request_id, "prove-it");
            assert_eq!(action.operation, "tasks.create");
            assert_eq!(action.target, None);
            assert_eq!(action.input, json!({"title":"prove it"}));
        }
        other => panic!("expected AgentAction, got {other:?}"),
    }
}

#[test]
fn peer_call_exposes_no_caller_identity_or_authority_input() {
    let (result, captured) = capture_action(
        "ducktape_action",
        json!({
            "operation":"agent.call",
            "target":{"agent_id":"reviewer"},
            "input":{"instruction":"Review this change", "skills":["/shared/skills/review"]},
            "request_id":"review-1",
        }),
    );
    assert_ne!(result["isError"], true, "{result}");
    match captured.message {
        runs::RunsMsg::AgentAction {
            run_id,
            request_id,
            action,
        } => {
            assert_eq!(run_id, RUN_ID);
            assert_eq!(request_id, "review-1");
            assert_eq!(action.operation, "agent.call");
            assert_eq!(action.target, Some(json!({"agent_id":"reviewer"})));
            assert_eq!(
                action.input,
                json!({"instruction":"Review this change", "skills":["/shared/skills/review"]})
            );
        }
        other => panic!("expected AgentAction, got {other:?}"),
    }
}

#[test]
fn a_run_without_a_scoped_endpoint_refuses_to_write() {
    let result = call_tool(
        command(None),
        "ducktape_action",
        json!({"operation":"tasks.create", "input":{"title":"nope"}, "request_id":"nope"}),
    );
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("no scoped action endpoint")
    );
}

fn read_request(stream: &mut TcpStream) -> (String, Vec<(String, String)>, Vec<u8>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut start = String::new();
    reader.read_line(&mut start).expect("request line");
    let path = start.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut headers = Vec::new();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header");
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).expect("request body");
    (path, headers, body)
}

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = value.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write response");
}
