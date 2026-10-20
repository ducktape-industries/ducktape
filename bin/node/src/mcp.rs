//! `ducktape mcp`: the Model Context Protocol over stdin and stdout, one JSON-RPC message a line,
//! serving borsh-json's network tools in its own words plus files_read and files_write, every op
//! and query signed by the active wallet key; `live.rs` is the node it speaks to.
mod live;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use abi::role::identity::{AccountNumber, Kind, Profile};
use abi::{BlobId, Outcome};
use borsh_json::discover::{self, Ask, Refusal, Side};
use borsh_json::spoken;
use futures::StreamExt as _;
use futures::stream::FuturesUnordered;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _};

pub use live::Live;

use crate::json::Code;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

const REVISIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST: &str = "2025-06-18";

const AGENTS_ONLY: &str =
    "ducktape mcp signs only as an agent account, so a model never holds a person's key";

const FILES: &str = files::PROGRAM;
const READ: &str = "files_read";
const WRITE: &str = "files_write";

const READ_DESCRIPTION: &str = "Read a file of the network's filesystem as text, from an offset, as much as files answers in one read; `eof` says whether the file goes on. The answer names the snapshot it read, which files_write can take as its base.";

const WRITE_DESCRIPTION: &str = "Write a text file to the network's filesystem as your own account, creating its directories, in one commit: its bytes go up through the node as chunk blobs, then files commits them. A file already there keeps its meta, and its mode unless executable is given. Anyone writes a path that no write list covers; where the path or a folder above it carries a write list, the nearest list decides, and a refusal names it. /home, /shared and each /home/<name> folder itself are the network's; write inside them. Answers the new snapshot and the block that carried it.";

pub(crate) struct Landed {
    pub height: u64,
    pub outcome: Outcome,
}

pub(crate) fn agent(
    key: &str,
    profile: Option<Profile>,
) -> Result<(Profile, AccountNumber), String> {
    let Some(profile) = profile else {
        return Err(format!(
            "the key {key} acts as no account. {AGENTS_ONLY}: make one with identity's CreateAgent from your own account, give it this key with identity's AddKey (`ducktape wallet consent --account <agent>` with this key active signs its consent), then start ducktape mcp again"
        ));
    };
    let (number, name) = (profile.number, profile.name.clone());
    match &profile.kind {
        Kind::Program { manager, .. } => {
            let manager = *manager;
            Ok((profile, manager))
        }
        Kind::User => Err(format!(
            "the key {key} acts as account {number} ({name}), a person's own account. {AGENTS_ONLY}: make an agent with identity's CreateAgent and give it a key of its own with AddKey"
        )),
        Kind::System(program) => Err(format!(
            "the key {key} acts as account {number}, program {program}'s own account. {AGENTS_ONLY}"
        )),
    }
}

pub(crate) trait Network {
    async fn whom(&self) -> String;
    async fn running(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<(String, Result<Code, String>)>, String>;
    async fn query(&self, program: &str, request: Vec<u8>) -> Result<Vec<u8>, String>;
    async fn send(&self, program: &str, payload: Vec<u8>) -> Result<Landed, String>;
    async fn egress(&self) -> Result<Option<String>, String>;
    async fn put(&self, kind: &str, bodies: Vec<Vec<u8>>) -> Result<Vec<BlobId>, String>;
}

pub(crate) async fn serve<N: Network>(
    network: &N,
    input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> std::io::Result<()> {
    let mut lines = input.lines();
    let mut pending = FuturesUnordered::new();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else {
                    break;
                };
                pending.push(answer(network, line));
            }
            Some(answered) = pending.next() => written(&mut output, answered).await?,
        }
    }
    while let Some(answered) = pending.next().await {
        written(&mut output, answered).await?;
    }
    Ok(())
}

async fn written(
    output: &mut (impl AsyncWrite + Unpin),
    answered: Option<Value>,
) -> std::io::Result<()> {
    let Some(answered) = answered else {
        return Ok(());
    };
    let mut line = answered.to_string();
    line.push('\n');
    output.write_all(line.as_bytes()).await?;
    output.flush().await
}

async fn answer<N: Network>(network: &N, line: String) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    let message: Value = match serde_json::from_str(&line) {
        Ok(message) => message,
        Err(fault) => {
            let why = format!("the message is no JSON: {fault}");
            return Some(error(Value::Null, PARSE_ERROR, why));
        }
    };
    let id = message.get("id").cloned()?;
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let a_response = message.get("result").is_some() || message.get("error").is_some();
        return match a_response {
            true => None,
            false => Some(error(
                id,
                INVALID_REQUEST,
                "a request names its method".into(),
            )),
        };
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let answered = match method {
        "initialize" => Ok(initialize(network, &params).await),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(network, params).await,
        other => Err((METHOD_NOT_FOUND, format!("there is no method {other}"))),
    };
    Some(match answered {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, why)) => error(id, code, why),
    })
}

fn error(id: Value, code: i64, why: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": why } })
}

async fn initialize<N: Network>(network: &N, params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let revision = asked
        .filter(|asked| REVISIONS.contains(asked))
        .unwrap_or(LATEST);
    json!({
        "protocolVersion": revision,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "ducktape", "version": env!("CARGO_PKG_VERSION") },
        "instructions": network.whom().await,
    })
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            discover::SCHEMA,
            discover::SCHEMA_DESCRIPTION,
            discover::schema_input(),
        ),
        tool(
            discover::QUERY,
            discover::QUERY_DESCRIPTION,
            discover::query_input(),
        ),
        tool(
            discover::CALL,
            discover::CALL_DESCRIPTION,
            discover::call_input(),
        ),
        tool(READ, READ_DESCRIPTION, read_input()),
        tool(WRITE, WRITE_DESCRIPTION, write_input()),
    ]
}

fn tool(name: &str, description: &str, input: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input })
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

fn snapshot_input() -> Value {
    json!({ "type": "string", "description": "a snapshot id (64 hex digits) to read at; omitted, the newest" })
}

fn read_input() -> Value {
    object(
        json!({
            "path": { "type": "string", "description": "an absolute file path" },
            "snapshot": snapshot_input(),
            "offset": { "type": "integer", "minimum": 0, "description": "the first byte to read; 0 if omitted" },
            "length": { "type": "integer", "minimum": 1, "description": "how many bytes; the rest of the file if omitted" },
        }),
        &["path"],
    )
}

fn write_input() -> Value {
    object(
        json!({
            "path": { "type": "string", "description": "an absolute file path" },
            "content": { "type": "string", "description": "the whole file" },
            "executable": { "type": "boolean", "description": "whether the file is executable; omitted, a file already there keeps its mode and a new one is not" },
            "base": { "type": "string", "description": "the snapshot your edit is based on, from a read; the edit is refused if a path it touches changed since. Omitted, it applies over the newest" },
            "message": { "type": "string", "description": "the commit message" },
        }),
        &["path", "content"],
    )
}

fn nothing() -> Value {
    json!({})
}

#[derive(Deserialize)]
struct Called {
    name: String,
    #[serde(default = "nothing")]
    arguments: Value,
}

async fn call<N: Network>(network: &N, params: Value) -> Result<Value, (i64, String)> {
    let called: Called = serde_json::from_value(params).map_err(|fault| {
        let why = format!("tools/call takes a tool's name and its arguments: {fault}");
        (INVALID_PARAMS, why)
    })?;
    let arguments = called.arguments;
    let ran = match called.name.as_str() {
        discover::SCHEMA => schema(network, arguments).await,
        discover::QUERY => query(network, arguments).await,
        discover::CALL => send(network, arguments).await,
        READ => read(network, arguments).await,
        WRITE => write(network, arguments).await,
        other => {
            let names: Vec<Value> = tools()
                .into_iter()
                .map(|tool| tool["name"].clone())
                .collect();
            let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
            let why = format!(
                "there is no tool {other}; the tools are {}",
                names.join(", ")
            );
            return Err((INVALID_PARAMS, why));
        }
    };
    Ok(match ran {
        Ok(value) => json!({ "content": [{ "type": "text", "text": value.to_string() }] }),
        Err(why) => json!({ "content": [{ "type": "text", "text": why }], "isError": true }),
    })
}

fn args<T: DeserializeOwned>(tool: &str, arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments)
        .map_err(|fault| format!("{tool}'s arguments do not read: {fault}"))
}

fn declined(refusal: Refusal) -> String {
    match refusal {
        Refusal::Asked(why) | Refusal::Corrupt(why) => why,
    }
}

async fn one<N: Network>(network: &N, program: &str) -> Result<Code, String> {
    network
        .running(Some(program))
        .await?
        .into_iter()
        .find(|(running, _)| running == program)
        .map(|(_, code)| code)
        .ok_or_else(|| format!("no program named {program} runs on this network"))?
}

#[derive(Deserialize)]
struct SchemaArgs {
    program: Option<String>,
    op: Option<String>,
    query: Option<String>,
    whole: Option<bool>,
}

async fn schema<N: Network>(network: &N, arguments: Value) -> Result<Value, String> {
    let asked: SchemaArgs = args(discover::SCHEMA, arguments)?;
    match Ask::of(asked.program, asked.op, asked.query, asked.whole)? {
        Ask::Roster => roster(network).await,
        Ask::Index(program) => index(network, &program).await,
        Ask::Whole(program) => whole(network, &program).await,
        Ask::Op(program, op) => unfolded(network, &program, Side::Op, &op).await,
        Ask::Query(program, query) => unfolded(network, &program, Side::Query, &query).await,
    }
}

async fn roster<N: Network>(network: &N) -> Result<Value, String> {
    let told = network
        .running(None)
        .await?
        .into_iter()
        .map(|(program, code)| (program, code.and_then(|code| code.told)))
        .collect();
    Ok(discover::roster(told))
}

async fn index<N: Network>(network: &N, program: &str) -> Result<Value, String> {
    let code = one(network, program).await?;
    Ok(discover::index(program, &code.interface?, &code.told?))
}

async fn whole<N: Network>(network: &N, program: &str) -> Result<Value, String> {
    let interface = one(network, program).await?.interface?;
    discover::whole(program, &interface).map_err(declined)
}

async fn unfolded<N: Network>(
    network: &N,
    program: &str,
    side: Side,
    name: &str,
) -> Result<Value, String> {
    let code = one(network, program).await?;
    discover::unfold(program, &code.interface?, &code.told?, side, name).map_err(declined)
}

#[derive(Deserialize)]
struct QueryArgs {
    program: String,
    query: Value,
}

async fn query<N: Network>(network: &N, arguments: Value) -> Result<Value, String> {
    let asked: QueryArgs = args(discover::QUERY, arguments)?;
    let interface = one(network, &asked.program).await?.interface?;
    let request = interface
        .encode_query(&spoken(&asked.query))
        .map_err(|fault| fault.to_string())?;
    let answer = network.query(&asked.program, request).await?;
    Ok(discover::replied(&interface, &answer))
}

#[derive(Deserialize)]
struct CallArgs {
    program: String,
    op: Value,
}

async fn send<N: Network>(network: &N, arguments: Value) -> Result<Value, String> {
    let asked: CallArgs = args(discover::CALL, arguments)?;
    let interface = one(network, &asked.program).await?.interface?;
    let payload = interface
        .encode_op(&spoken(&asked.op))
        .map_err(|fault| fault.to_string())?;
    let bound = network.egress().await?;
    let to_egress = bound.is_some_and(|bound| bound == asked.program);
    let landed = network.send(&asked.program, payload.clone()).await?;
    let output = applied(landed.outcome)?;
    let mut answer = match to_egress && discover::asks_http(&payload) {
        true => discover::http_answer(&output),
        false => interface.returned(&asked.program, &payload, &output),
    };
    if let Value::Object(answer) = &mut answer {
        answer.insert("landed".into(), json!(landed.height));
    }
    Ok(answer)
}

fn applied(outcome: Outcome) -> Result<Vec<u8>, String> {
    match outcome {
        Outcome::Applied { output } => Ok(output),
        Outcome::Rejected(refusal) => {
            Err(format!("rejected {}: {}", refusal.reason, refusal.sentence))
        }
    }
}

fn snapshot(text: Option<&str>) -> Result<Option<BlobId>, String> {
    let Some(text) = text else {
        return Ok(None);
    };
    let digest: [u8; 32] = hex::decode(text)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("a snapshot is 64 hex digits")?;
    Ok(Some(BlobId::Sha256(digest)))
}

fn shown(id: &BlobId) -> Value {
    json!(abi::hex(id.digest()))
}

async fn ask<N: Network>(network: &N, query: &files::Query) -> Result<files::Reply, String> {
    let answer = network.query(FILES, abi::encode(query)).await?;
    abi::decode(&answer)
        .map_err(|refusal| format!("program {FILES} answered no reply: {}", refusal.sentence))
}

fn unexpected(asked: &str) -> String {
    format!("program {FILES} answered {asked} with something else")
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    snapshot: Option<String>,
    offset: Option<u64>,
    length: Option<u64>,
}

async fn read<N: Network>(network: &N, arguments: Value) -> Result<Value, String> {
    let asked: ReadArgs = args(READ, arguments)?;
    let query = files::Query::Read {
        path: asked.path,
        at: snapshot(asked.snapshot.as_deref())?,
        offset: asked.offset.unwrap_or(0),
        len: asked.length.unwrap_or(u64::MAX),
    };
    let files::Reply::Contents(contents) = ask(network, &query).await? else {
        return Err(unexpected("a read"));
    };
    let mut value = json!({
        "snapshot": shown(&contents.snapshot),
        "size": contents.size,
        "offset": contents.offset,
        "eof": contents.eof,
    });
    match String::from_utf8(contents.bytes) {
        Ok(text) => value["text"] = json!(text),
        Err(bytes) => value["binary_bytes"] = json!(bytes.into_bytes().len()),
    }
    Ok(value)
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
    executable: Option<bool>,
    base: Option<String>,
    message: Option<String>,
}

async fn write<N: Network>(network: &N, arguments: Value) -> Result<Value, String> {
    let asked: WriteArgs = args(WRITE, arguments)?;
    let base = match snapshot(asked.base.as_deref())? {
        Some(base) => Some(base),
        None => head(network).await?,
    };
    let (exec, meta) = kept(network, &asked.path, base).await?;
    let bytes = asked.content.into_bytes();
    let bodies: Vec<Vec<u8>> = bytes
        .chunks(files::CHUNK_SIZE as usize)
        .map(<[u8]>::to_vec)
        .collect();
    let chunks = match bodies.is_empty() {
        true => Vec::new(),
        false => network.put(files::CHUNK, bodies).await?,
    };
    let message = asked
        .message
        .unwrap_or_else(|| format!("ducktape mcp writes {}", asked.path));
    let put = files::Change::Put {
        path: asked.path,
        exec: asked.executable.unwrap_or(exec),
        meta,
        content: files::Content::Chunks {
            size: bytes.len() as u64,
            chunks,
        },
    };
    let commit = files::Op::Commit {
        base,
        message,
        changes: vec![put],
    };
    let landed = network.send(FILES, abi::encode(&commit)).await?;
    let output = applied(landed.outcome)?;
    let snapshot: BlobId = abi::decode(&output).map_err(|refusal| {
        format!(
            "program {FILES}'s commit returned no snapshot: {}",
            refusal.sentence
        )
    })?;
    Ok(json!({ "snapshot": shown(&snapshot), "landed": landed.height }))
}

async fn head<N: Network>(network: &N) -> Result<Option<BlobId>, String> {
    let files::Reply::Head(head) = ask(network, &files::Query::Head).await? else {
        return Err(unexpected("Head"));
    };
    Ok(head.map(|row| row.id))
}

async fn kept<N: Network>(
    network: &N,
    path: &str,
    at: Option<BlobId>,
) -> Result<(bool, BTreeMap<String, String>), String> {
    let asked = files::Query::Stat {
        path: path.to_owned(),
        at,
    };
    let files::Reply::Stat { entry, .. } = ask(network, &asked).await? else {
        return Err(unexpected("a stat"));
    };
    Ok(match entry {
        Some(files::EntryInfo {
            node: files::Node::File { exec, .. },
            meta,
            ..
        }) => (exec, meta),
        _ => (false, BTreeMap::new()),
    })
}
