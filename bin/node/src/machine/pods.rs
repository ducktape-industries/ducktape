//! The `pods` backend: each step of a run (make the pod, wait for its end, gather its outputs,
//! delete it) is one egress call to the pods service that fleet relays for the machine, and each
//! answer is read back from the run's `p/` rows as fleet writes them.
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use abi::role::egress::{DEFAULT_WAIT, http};
use noded::Upload;
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};

use super::backend::{Input, Kill, Workload, blob_json, blob_of, blocking, variant};
use super::decide::Ending;
use super::{Done, Failure, Landing, Shared, Talk, Task, out};
use crate::Sentence as _;

const PART: &str = "part";
const CHUNK: &str = "chunk";
const OUT: &str = "out";
const KEEP: &str = "/out/.fleet";
const SCRATCH: &str = "/tmp/fleet";
const FILLS: &str = "/fleet/fills";
const ENDED: &str = "ended";
const FETCH: &str = "fetch";

const GATHER: &str = r#"import json
import os
import re
import shutil
import subprocess
import sys

size, keep, scratch = int(sys.argv[1]), sys.argv[2], sys.argv[3]
out = os.path.dirname(keep)
cut = os.path.join(scratch, "pieces")
packed = os.path.join(scratch, "packs")
for made in (cut, packed):
    shutil.rmtree(made, ignore_errors=True)
    os.makedirs(made)
held = {"files": [], "packs": [], "refused": []}
count = [0]


def pieces(path):
    index, names = count[0], []
    count[0] += 1
    with open(path, "rb") as source:
        while True:
            data = source.read(size)
            if not data:
                return {"size": os.path.getsize(path), "pieces": names}
            name = os.path.join(cut, f"{index}-{len(names)}")
            with open(name, "wb") as piece:
                piece.write(data)
            names.append(name)


def gather(name, path):
    try:
        held["files"].append({"name": name, **pieces(path)})
    except OSError as error:
        held["refused"].append(f"{name}: not read: {error.strerror}")


def unlisted(error):
    held["refused"].append(f"out/{os.path.relpath(error.filename, out)}: not read: {error.strerror}")


log = os.path.join(keep, "log")
if os.path.isfile(log):
    gather("log", log)
for at, dirs, files in os.walk(out, onerror=unlisted):
    dirs[:] = sorted(name for name in dirs if os.path.join(at, name) != keep and not os.path.islink(os.path.join(at, name)))
    for name in sorted(files):
        path = os.path.join(at, name)
        if os.path.isfile(path) and not os.path.islink(path):
            gather("out/" + os.path.relpath(path, out), path)
REPO = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]*")
OID = re.compile(r"[0-9a-f]{40}|[0-9a-f]{64}")
commits = os.path.join(out, "commits")
try:
    lines = open(commits).read().splitlines() if os.path.isfile(commits) else []
except OSError as error:
    lines = []
    unread = f"out/commits: not read: {error.strerror}"
    if unread not in held["refused"]:
        held["refused"].append(unread)
for line in lines:
    if not line.strip():
        continue
    words = line.split(" ", 2)
    if len(words) != 3 or not REPO.fullmatch(words[0]) or not OID.fullmatch(words[1]) or not words[2].startswith("/"):
        held["refused"].append(f"out/commits line {line!r} is no `<repo> <commit> <path>`")
        continue
    repo, commit, path = words
    git = ["git", "-c", "safe.directory=*", "-C", path]
    refs = subprocess.run([*git, "for-each-ref", "--format=^%(objectname)", "refs/remotes"], capture_output=True)
    if refs.returncode != 0:
        held["refused"].append(f"{repo} {commit}: {refs.stderr.decode(errors='replace').strip()}")
        continue
    pack = os.path.join(packed, f"{len(held['packs'])}.pack")
    with open(pack, "wb") as into:
        ran = subprocess.run([*git, "pack-objects", "--revs", "--stdout", "-q"], input=f"{commit}\n".encode() + refs.stdout, stdout=into, stderr=subprocess.PIPE)
    if ran.returncode != 0:
        held["refused"].append(f"{repo} {commit}: {ran.stderr.decode(errors='replace').strip()}")
        continue
    held["packs"].append({"repo": repo, "commit": commit, **pieces(pack)})
json.dump(held, sys.stdout)
"#;

#[derive(Clone, Debug, Default)]
pub struct Calls(Arc<AtomicU64>);

impl Calls {
    pub fn draw(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

impl PartialEq for Calls {
    fn eq(&self, other: &Calls) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Calls {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Answered {
        status: u16,
        body: Vec<u8>,
        events: Vec<Vec<u8>>,
    },
    Failed {
        why: String,
        events: Vec<Vec<u8>>,
    },
}

impl Reply {
    fn events(&self) -> &[Vec<u8>] {
        match self {
            Reply::Answered { events, .. } | Reply::Failed { events, .. } => events,
        }
    }
}

pub trait Relay {
    fn call(
        &mut self,
        method: http::Method,
        target: String,
        body: Option<Value>,
        stream: bool,
    ) -> impl Future<Output = Result<Reply, String>> + Send;

    fn pack(
        &mut self,
        repo: &str,
        commit: &str,
    ) -> impl Future<Output = Result<bool, String>> + Send;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fill {
    pub input: usize,
    pub path: String,
    pub commit: String,
    pub pack: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gathered {
    pub manifest: Value,
    pub blobs: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Flowed {
    Unmade(String),
    Stopped(Ending),
    Ended {
        state: &'static str,
        summary: String,
        gathered: Option<Gathered>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct End {
    pub state: &'static str,
    pub summary: String,
    pub fetch_failed: bool,
}

enum Waited {
    Ended(Value),
    Stopped(Ending),
    Unread(String),
}

pub fn every(block_ms: u64) -> Result<u64, String> {
    let lasts = DEFAULT_WAIT.saturating_mul(block_ms);
    if lasts < 2000 {
        return Err(format!(
            "a request lasts {lasts} ms ({DEFAULT_WAIT} blocks of {block_ms} ms), under the 2 s two whole-second beats need"
        ));
    }
    Ok(lasts / 2 / 1000)
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn fill_at(fill: &Fill) -> String {
    format!("{FILLS}/{}.pack", fill.input)
}

fn wrapper(fills: &[Fill]) -> String {
    let mut script = format!("set -e\nmkdir -p {KEEP}\nexec > {KEEP}/log 2>&1\n");
    for fill in fills {
        let git = format!("git -c safe.directory='*' -C {}", quoted(&fill.path));
        script.push_str(&format!(
            "{git} index-pack --stdin --fix-thin < {}\n{git} -c advice.detachedHead=false checkout --quiet {}\n",
            quoted(&fill_at(fill)),
            quoted(&fill.commit)
        ));
    }
    script.push_str("exec \"$@\"\n");
    script
}

fn input_json(index: usize, input: &Input, fills: &[Fill]) -> Result<Value, String> {
    match input {
        Input::Forge { path, repo, commit } => {
            let filled = fills.iter().any(|fill| fill.input == index);
            let at = if filled { "HEAD" } else { commit.as_str() };
            Ok(json!({ "path": path, "forge": { "repo": repo, "commit": at } }))
        }
        Input::Files {
            path,
            files,
            snapshot,
        } => {
            let mut read = json!({ "path": files });
            if !snapshot.is_null() {
                read["snapshot"] = json!(crate::blob_text(&blob_of(snapshot)?));
            }
            Ok(json!({ "path": path, "files": read }))
        }
        Input::Blob { path, blob } => Ok(json!({ "path": path, "blob": crate::blob_text(blob) })),
        Input::Scratch {
            path,
            memory: true,
            size,
        } => {
            let mut scratch = json!({ "medium": "Memory" });
            if let Some(size) = size {
                scratch["size"] = json!(size);
            }
            Ok(json!({ "path": path, "scratch": scratch }))
        }
        Input::Scratch { path, .. } => Err(format!(
            "scratch {path} asks for disk, and pods' scratch is memory only"
        )),
    }
}

pub fn pod_body(workload: &Workload, fills: &[Fill]) -> Result<Value, String> {
    let image = workload
        .image
        .as_ref()
        .ok_or("the pods service runs an image, and the workload names none")?;
    if workload.command.is_empty() {
        return Err("the workload names no command".into());
    }
    let mut command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        wrapper(fills),
        "sh".to_owned(),
    ];
    command.extend(workload.command.iter().cloned());
    let mut env = serde_json::Map::new();
    env.insert("OUT".to_owned(), json!("/out"));
    for (name, value) in &workload.env {
        env.insert(name.clone(), json!(value));
    }
    let mut inputs = Vec::new();
    for (index, input) in workload.inputs.iter().enumerate() {
        inputs.push(input_json(index, input, fills)?);
    }
    for fill in fills {
        inputs.push(json!({ "path": fill_at(fill), "files": { "path": fill.pack } }));
    }
    let mut body = json!({
        "image": image,
        "command": command,
        "env": env,
        "inputs": inputs,
    });
    if let Some(workdir) = &workload.workdir {
        body["workdir"] = json!(workdir);
    }
    let mut requests = serde_json::Map::new();
    if let Some(cpu) = &workload.cpu {
        requests.insert("cpu".to_owned(), json!(cpu));
    }
    if let Some(memory) = &workload.memory {
        requests.insert("memory".to_owned(), json!(memory));
    }
    if !requests.is_empty() {
        body["resources"] = json!({ "requests": requests });
    }
    Ok(body)
}

fn method_word(method: http::Method) -> &'static str {
    match method {
        http::Method::Get => "get",
        http::Method::Post => "post",
        http::Method::Put => "put",
        http::Method::Patch => "patch",
        http::Method::Delete => "delete",
    }
}

pub struct Asked<'a> {
    pub service: &'a str,
    pub run: u64,
    pub attempt: u32,
    pub call: u64,
}

pub fn call_op(
    asked: &Asked,
    method: http::Method,
    target: &str,
    parts: Vec<Value>,
    stream: bool,
) -> Value {
    let headers: Vec<Value> = if parts.is_empty() {
        Vec::new()
    } else {
        vec![json!(["content-type", "application/json"])]
    };
    json!({ "call": {
        "run": asked.run,
        "attempt": asked.attempt,
        "call": asked.call,
        "request": { "http": {
            "service": asked.service,
            "head": { "method": method_word(method), "target": target, "headers": headers },
            "body": parts,
        }},
        "stream": stream,
        "wait": null,
    }})
}

pub fn settled(pieces: &[Value]) -> Result<Option<Reply>, String> {
    let mut events = Vec::new();
    for row in pieces {
        let (name, body) = variant(&row["piece"])?;
        match name {
            "event" => {
                let data = body["data"]
                    .as_str()
                    .ok_or("a piece's event holds no data")?;
                events.push(hex::decode(data).sentence()?);
            }
            "answer" => {
                let bytes = hex::decode(body.as_str().ok_or("a piece's answer holds no bytes")?)
                    .sentence()?;
                let response: http::Response =
                    abi::decode(&bytes).map_err(|refusal| refusal.sentence)?;
                return Ok(Some(Reply::Answered {
                    status: response.status,
                    body: response.body,
                    events,
                }));
            }
            "failed" => {
                let why = body.as_str().unwrap_or_default().to_owned();
                return Ok(Some(Reply::Failed { why, events }));
            }
            other => {
                return Err(format!(
                    "fleet wrote a piece this machine does not know: {other}"
                ));
            }
        }
    }
    Ok(None)
}

pub fn sse(events: &[Vec<u8>]) -> Vec<(String, Value)> {
    let joined: Vec<u8> = events.concat();
    let text = String::from_utf8_lossy(&joined).replace("\r\n", "\n");
    let mut said = Vec::new();
    for block in text.split("\n\n") {
        let mut kind = None;
        let mut data = Vec::new();
        for line in block.lines() {
            if let Some(name) = line.strip_prefix("event: ") {
                kind = Some(name.to_owned());
            }
            if let Some(line) = line.strip_prefix("data: ") {
                data.push(line);
            }
        }
        if let Some(kind) = kind {
            let data = serde_json::from_str(&data.join("\n")).unwrap_or(Value::Null);
            said.push((kind, data));
        }
    }
    said
}

fn ended_event(events: &[Vec<u8>]) -> Option<Value> {
    sse(events)
        .into_iter()
        .find(|(kind, _)| kind == ENDED)
        .map(|(_, data)| data)
}

fn container<'a>(view: &'a Value, group: &str, name: &str) -> Option<&'a Value> {
    view[group]
        .as_array()?
        .iter()
        .find(|container| container["name"] == name)
        .map(|container| &container["status"])
}

fn terminated(status: Option<&Value>) -> bool {
    status.is_some_and(|status| status["state"] == "terminated")
}

pub fn ended_view(view: &Value) -> Option<Value> {
    let name = view["name"].as_str().unwrap_or_default();
    let ran = container(view, "containers", "run");
    let fetched = container(view, "init", FETCH);
    let fetch_failed = terminated(fetched)
        && fetched.is_some_and(|status| status["exit_code"].as_i64() != Some(0));
    let line = if terminated(ran) {
        let status = ran?;
        format!("pod {name} ended: exit code {}", status["exit_code"])
    } else if fetch_failed {
        format!("pod {name} ended: fetching its inputs failed")
    } else if view["phase"] == "Failed" {
        format!(
            "pod {name} ended: {}",
            view["reason"].as_str().unwrap_or("failed")
        )
    } else {
        return None;
    };
    let exit_code = match ran {
        Some(status) if terminated(Some(status)) => status["exit_code"].clone(),
        _ => Value::Null,
    };
    Some(json!({ "line": line, "exit_code": exit_code, "pod": view }))
}

pub fn ended(data: &Value) -> End {
    let state = match data["exit_code"].as_i64() {
        Some(0) => "passed",
        Some(_) => "failed",
        None => "errored",
    };
    let summary = data["line"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "the pod ended".to_owned());
    let fetched = container(&data["pod"], "init", FETCH);
    let fetch_failed = terminated(fetched)
        && fetched.is_some_and(|status| status["exit_code"].as_i64() != Some(0));
    End {
        state,
        summary,
        fetch_failed,
    }
}

fn error_of(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

fn whole(reply: Reply, what: &str) -> Result<(u16, Vec<u8>), String> {
    match reply {
        Reply::Answered { status, body, .. } => Ok((status, body)),
        Reply::Failed { why, .. } => Err(format!("{what} failed: {why}")),
    }
}

fn answered(reply: Reply, what: &str) -> Result<Value, String> {
    let (status, body) = whole(reply, what)?;
    if !(200..300).contains(&status) {
        return Err(format!("{what} answered {status}: {}", error_of(&body)));
    }
    serde_json::from_slice(&body).map_err(|error| format!("{what} answered no JSON: {error}"))
}

async fn made<R: Relay>(relay: &mut R, body: Value) -> Result<String, String> {
    let what = "the pod's creation";
    let reply = relay
        .call(http::Method::Post, "/v1/pods".into(), Some(body), false)
        .await?;
    let created = answered(reply, what)?;
    created["pod"]["name"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{what} named no pod"))
}

async fn status_read<R: Relay>(relay: &mut R, pod: &str) -> Result<Option<Value>, String> {
    let what = format!("pod {pod}'s read");
    let reply = relay
        .call(http::Method::Get, format!("/v1/pods/{pod}"), None, false)
        .await?;
    let read = answered(reply, &what)?;
    Ok(ended_view(&read["pod"]))
}

async fn waited<R: Relay>(
    relay: &mut R,
    pod: &str,
    every: u64,
    stop: &mut oneshot::Receiver<Ending>,
) -> Waited {
    loop {
        let target = format!("/v1/pods/{pod}/wait?every={every}");
        let reply = tokio::select! {
            reply = relay.call(http::Method::Get, target, None, true) => reply,
            ending = &mut *stop => return Waited::Stopped(ending.unwrap_or(Ending::Dropped)),
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(why) => return Waited::Unread(why),
        };
        if let Some(ended) = ended_event(reply.events()) {
            return Waited::Ended(ended);
        }
        if let Reply::Answered { status, events, .. } = &reply {
            let refused = *status != 200;
            if refused {
                let why = format!("the wait answered {status}: {}", error_of(&events.concat()));
                return Waited::Unread(why);
            }
        }
        match status_read(relay, pod).await {
            Ok(Some(ended)) => return Waited::Ended(ended),
            Ok(None) => {}
            Err(why) => return Waited::Unread(why),
        }
    }
}

pub fn pieces_of(manifest: &Value) -> Vec<String> {
    let groups = [&manifest["files"], &manifest["packs"]];
    groups
        .into_iter()
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|held| held["pieces"].as_array())
        .flatten()
        .filter_map(|piece| piece.as_str().map(str::to_owned))
        .collect()
}

async fn gathered<R: Relay>(relay: &mut R, pod: &str) -> Result<Gathered, String> {
    let exec = json!({
        "command": ["python3", "-c", GATHER, out::CHUNK_SIZE.to_string(), KEEP, SCRATCH],
        "container": OUT,
    });
    let target = format!("/v1/pods/{pod}/exec");
    let reply = relay
        .call(http::Method::Post, target, Some(exec), false)
        .await?;
    let ran = answered(reply, "gathering the outputs")?;
    if ran["exit_code"].as_i64() != Some(0) {
        return Err(format!(
            "gathering the outputs exited {}: {}",
            ran["exit_code"],
            ran["stderr"].as_str().unwrap_or_default().trim()
        ));
    }
    let printed = ran["stdout"]
        .as_str()
        .ok_or("gathering the outputs printed no text")?;
    let manifest: Value = serde_json::from_str(printed)
        .map_err(|error| format!("gathering the outputs printed no manifest: {error}"))?;
    let paths = pieces_of(&manifest);
    if paths.is_empty() {
        return Ok(Gathered {
            manifest,
            blobs: Vec::new(),
        });
    }
    let put = json!({ "paths": paths, "kind": CHUNK, "container": OUT });
    let target = format!("/v1/pods/{pod}/blob");
    let reply = relay
        .call(http::Method::Post, target, Some(put), false)
        .await?;
    let held = answered(reply, "putting the outputs")?;
    let blobs = held["blobs"]
        .as_array()
        .cloned()
        .ok_or("putting the outputs named no blobs")?;
    Ok(Gathered { manifest, blobs })
}

async fn deleted<R: Relay>(relay: &mut R, pod: &str) -> Option<String> {
    let what = format!("pod {pod}'s deletion");
    let reply = relay
        .call(http::Method::Delete, format!("/v1/pods/{pod}"), None, false)
        .await;
    let failed = reply.and_then(|reply| answered(reply, &what)).err()?;
    Some(format!("pod {pod} was not deleted: {failed}"))
}

async fn fillable<R: Relay>(relay: &mut R, workload: &Workload) -> Vec<Fill> {
    let mut fills = Vec::new();
    for (index, input) in workload.inputs.iter().enumerate() {
        let Input::Forge { path, repo, commit } = input else {
            continue;
        };
        if relay.pack(repo, commit).await == Ok(true) {
            fills.push(Fill {
                input: index,
                path: path.clone(),
                commit: commit.clone(),
                pack: out::pack_path(repo, commit),
            });
        }
    }
    fills
}

fn noted(summary: String, note: Option<String>) -> String {
    match note {
        Some(note) => format!("{summary}; {note}"),
        None => summary,
    }
}

pub async fn flow<R: Relay>(
    relay: &mut R,
    workload: &Workload,
    every: u64,
    mut stop: oneshot::Receiver<Ending>,
    started: &mut (impl FnMut(&str) + Send),
) -> Flowed {
    let mut fills = Vec::new();
    loop {
        let body = match pod_body(workload, &fills) {
            Ok(body) => body,
            Err(why) => return Flowed::Unmade(why),
        };
        let pod = match made(relay, body).await {
            Ok(pod) => pod,
            Err(why) => return Flowed::Unmade(why),
        };
        started(&pod);
        let said = match waited(relay, &pod, every, &mut stop).await {
            Waited::Ended(said) => said,
            Waited::Stopped(ending) => {
                deleted(relay, &pod).await;
                return Flowed::Stopped(ending);
            }
            Waited::Unread(why) => {
                let left = deleted(relay, &pod).await;
                return Flowed::Ended {
                    state: "errored",
                    summary: noted(why, left),
                    gathered: None,
                };
            }
        };
        let end = ended(&said);
        let refill = end.fetch_failed && fills.is_empty();
        if refill {
            let found = fillable(relay, workload).await;
            if !found.is_empty() {
                deleted(relay, &pod).await;
                fills = found;
                continue;
            }
        }
        let gathering = gathered(relay, &pod).await;
        let left = deleted(relay, &pod).await;
        let (gathered, missed) = match gathering {
            Ok(gathered) => (Some(gathered), None),
            Err(why) => (None, Some(format!("outputs not gathered: {why}"))),
        };
        let summary = noted(noted(end.summary, missed), left);
        return Flowed::Ended {
            state: end.state,
            summary,
            gathered,
        };
    }
}

pub fn changes(gathered: &Gathered, folder: &str) -> Result<(Vec<Value>, Vec<String>), String> {
    let mut held = std::collections::BTreeMap::new();
    for blob in &gathered.blobs {
        let path = blob["path"].as_str();
        let id = blob["blob"].as_str().map(crate::blob_id).transpose()?;
        let (Some(path), Some(id)) = (path, id) else {
            return Err(format!("the pods service answered {blob} for a blob"));
        };
        held.insert(path.to_owned(), blob_json(&id));
    }
    let chunks = |listed: &Value| -> Result<(u64, Vec<Value>), String> {
        let size = listed["size"].as_u64().ok_or("an output has no size")?;
        let pieces = listed["pieces"]
            .as_array()
            .ok_or("an output has no pieces")?;
        let mut chunks = Vec::new();
        for piece in pieces {
            let piece = piece.as_str().unwrap_or_default();
            let id = held
                .get(piece)
                .ok_or_else(|| format!("piece {piece} was not put"))?;
            chunks.push(id.clone());
        }
        Ok((size, chunks))
    };
    let mut changes = Vec::new();
    let files = gathered.manifest["files"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for file in &files {
        let name = file["name"].as_str().ok_or("an output has no name")?;
        let (size, chunks) = chunks(file)?;
        let path = format!("{}/{name}", folder.trim_end_matches('/'));
        changes.push(out::put_change(path, size, chunks));
    }
    let packs = gathered.manifest["packs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for pack in &packs {
        let repo = pack["repo"].as_str().ok_or("a pack has no repo")?;
        let commit = pack["commit"].as_str().ok_or("a pack has no commit")?;
        let (size, chunks) = chunks(pack)?;
        changes.push(out::put_change(out::pack_path(repo, commit), size, chunks));
    }
    let refused = gathered.manifest["refused"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|line| line.as_str().map(str::to_owned))
        .collect();
    Ok((changes, refused))
}

struct Fleeted {
    shared: Arc<Shared>,
    service: String,
    run: u64,
    attempt: u32,
    calls: Calls,
    heard: watch::Receiver<u64>,
    dir: PathBuf,
}

impl Fleeted {
    async fn part(&self, call: u64, body: &Value) -> Result<Value, String> {
        let bytes = serde_json::to_vec(body).sentence()?;
        let path = self.dir.join(format!("part-{call}"));
        let at = path.clone();
        let entry = blocking(move || {
            std::fs::write(&at, &bytes).sentence()?;
            blob::file_entry(PART, &at).map_err(|refusal| refusal.sentence)
        })
        .await?;
        let id = blob_json(&entry.id);
        let upload = Upload {
            kind: PART.to_owned(),
            path: path.clone(),
        };
        let put = out::put(&self.shared.talk, vec![entry], vec![upload]).await;
        let _ = blocking(move || std::fs::remove_file(path).sentence()).await;
        put.map(|()| id)
    }

    async fn pieces(&self, call: u64) -> Result<Vec<Value>, String> {
        let mut rows = Vec::new();
        let mut after = Value::Null;
        loop {
            let query = json!({ "pieces": {
                "run": self.run,
                "attempt": self.attempt,
                "call": call,
                "page": { "after": after, "limit": null },
            }});
            let reply = self.shared.talk.fleet_ask(query).await?;
            let page = &reply["pieces"];
            let items = page["items"]
                .as_array()
                .ok_or("fleet answered Pieces with no pieces")?;
            rows.extend(items.iter().cloned());
            match &page["next"] {
                Value::Null => return Ok(rows),
                next => after = next.clone(),
            }
        }
    }

    async fn answer(&mut self, call: u64) -> Result<Reply, String> {
        loop {
            self.heard.mark_unchanged();
            let pieces = self.pieces(call).await?;
            if let Some(reply) = settled(&pieces)? {
                return Ok(reply);
            }
            self.heard
                .changed()
                .await
                .map_err(|_| "the machine stopped hearing fleet's writes".to_owned())?;
        }
    }
}

impl Relay for Fleeted {
    async fn call(
        &mut self,
        method: http::Method,
        target: String,
        body: Option<Value>,
        stream: bool,
    ) -> Result<Reply, String> {
        let call = self.calls.draw();
        let mut parts = Vec::new();
        if let Some(body) = body {
            parts.push(self.part(call, &body).await?);
        }
        let asked = Asked {
            service: &self.service,
            run: self.run,
            attempt: self.attempt,
            call,
        };
        let op = call_op(&asked, method, &target, parts, stream);
        self.shared.talk.fleet(op, Landing::Land).await?;
        self.answer(call).await
    }

    async fn pack(&mut self, repo: &str, commit: &str) -> Result<bool, String> {
        let stat = json!({ "stat": { "path": out::pack_path(repo, commit), "at": null } });
        let stated = self.shared.talk.files_ask(stat).await?;
        Ok(stated["stat"]["entry"]["node"]["file"].is_object())
    }
}

pub async fn abandoned(
    talk: &Talk,
    service: &str,
    run: u64,
    attempt: u32,
    pod: &str,
    calls: &Calls,
) {
    let asked = Asked {
        service,
        run,
        attempt,
        call: calls.draw(),
    };
    let target = format!("/v1/pods/{pod}");
    let op = call_op(&asked, http::Method::Delete, &target, Vec::new(), false);
    let _ = talk.fleet(op, Landing::Land).await;
}

pub async fn ran(
    task: &Task,
    workload: &Workload,
    dir: PathBuf,
    stop: oneshot::Receiver<Ending>,
) -> Result<(), Failure> {
    let (run, attempt) = (task.run, task.attempt);
    let made = dir.clone();
    if let Err(error) = blocking(move || super::backend::fresh(&made)).await {
        return task.report("errored", error, None).await;
    }
    let every = match every(task.shared.block_ms) {
        Ok(every) => every,
        Err(why) => return task.report("errored", why, None).await,
    };
    let calls = Calls::default();
    let mut relay = Fleeted {
        shared: task.shared.clone(),
        service: task.machine.service.clone().unwrap_or_default(),
        run,
        attempt,
        calls: calls.clone(),
        heard: task.shared.heard.subscribe(),
        dir: dir.clone(),
    };
    let done = task.done.clone();
    let mut started = |pod: &str| {
        let kill = Kill::Pod {
            name: pod.to_owned(),
            calls: calls.clone(),
        };
        let _ = done.send(Done::Started { run, attempt, kill });
    };
    let flowed = flow(&mut relay, workload, every, stop, &mut started).await;
    let _ = blocking(move || std::fs::remove_dir_all(dir).sentence()).await;
    let (state, summary, gathered) = match flowed {
        Flowed::Unmade(why) => return task.report("errored", why, None).await,
        Flowed::Stopped(Ending::Dropped) => return Ok(()),
        Flowed::Stopped(Ending::Stopped | Ending::Running) => {
            return task.report("cancelled", "stopped".to_owned(), None).await;
        }
        Flowed::Ended {
            state,
            summary,
            gathered,
        } => (state, summary, gathered),
    };
    let Some(gathered) = gathered else {
        return task.report(state, summary, None).await;
    };
    let folder = workload
        .out
        .clone()
        .unwrap_or_else(|| format!("/shared/fleet/{run}/"));
    let (changes, refused) = match changes(&gathered, &folder) {
        Ok(changes) => changes,
        Err(why) => {
            let kept = format!("{summary}; outputs not written: {why}");
            return task.report(state, kept, None).await;
        }
    };
    let summary = super::unpacked_summary(summary, &refused);
    let message = format!(
        "fleet run {run} attempt {attempt}: {}",
        state.to_lowercase()
    );
    match out::committed(&task.shared.talk, changes, &folder, message).await {
        Ok(written) => task.report(state, summary, Some(written)).await,
        Err(why) => {
            let kept = format!("{summary}; outputs not written: {why}");
            task.report(state, kept, None).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn workload() -> Workload {
        Workload {
            image: Some("toolchain".into()),
            command: vec!["make".into(), "check".into()],
            env: vec![("A".into(), "1".into())],
            workdir: Some("/src".into()),
            inputs: vec![
                Input::Forge {
                    path: "/src".into(),
                    repo: "core".into(),
                    commit: "ab".repeat(20),
                },
                Input::Files {
                    path: "/in/f".into(),
                    files: "/shared/f".into(),
                    snapshot: json!({ "sha256": "cd".repeat(32) }),
                },
                Input::Blob {
                    path: "/in/b".into(),
                    blob: abi::BlobId::Sha256([0; 32]),
                },
                Input::Scratch {
                    path: "/tmp".into(),
                    memory: true,
                    size: Some("1Gi".into()),
                },
            ],
            cpu: Some("2".into()),
            memory: Some("4Gi".into()),
            out: None,
        }
    }

    #[test]
    fn a_pod_is_the_workload_with_its_inputs_as_pods_names_them_and_no_requester() {
        let body = pod_body(&workload(), &[]).unwrap();
        assert_eq!(body["image"], "toolchain");
        let command = body["command"].as_array().unwrap();
        assert_eq!(command[..2], [json!("sh"), json!("-c")]);
        assert_eq!(command[3..], [json!("sh"), json!("make"), json!("check")]);
        let script = command[2].as_str().unwrap();
        assert!(script.contains("exec > /out/.fleet/log 2>&1"), "{script}");
        assert!(script.ends_with("exec \"$@\"\n"), "{script}");
        assert!(!script.contains("index-pack"), "{script}");
        assert_eq!(body["env"], json!({ "OUT": "/out", "A": "1" }));
        assert_eq!(body["workdir"], "/src");
        assert_eq!(
            body["inputs"],
            json!([
                { "path": "/src", "forge": { "repo": "core", "commit": "ab".repeat(20) } },
                { "path": "/in/f", "files": { "path": "/shared/f", "snapshot": format!("sha256:{}", "cd".repeat(32)) } },
                { "path": "/in/b", "blob": format!("sha256:{}", "00".repeat(32)) },
                { "path": "/tmp", "scratch": { "medium": "Memory", "size": "1Gi" } },
            ])
        );
        assert_eq!(
            body["resources"],
            json!({ "requests": { "cpu": "2", "memory": "4Gi" } })
        );
        assert!(body.get("requester").is_none(), "{body}");
        let bare = Workload {
            image: None,
            ..workload()
        };
        assert!(pod_body(&bare, &[]).is_err());
        let disk = Workload {
            inputs: vec![Input::Scratch {
                path: "/w".into(),
                memory: false,
                size: None,
            }],
            ..workload()
        };
        assert!(pod_body(&disk, &[]).unwrap_err().contains("memory"));
    }

    #[test]
    fn a_filled_pod_clones_the_default_branch_and_lays_the_pack_in_before_its_command() {
        let fill = Fill {
            input: 0,
            path: "/src".into(),
            commit: "ab".repeat(20),
            pack: out::pack_path("core", &"ab".repeat(20)),
        };
        let body = pod_body(&workload(), std::slice::from_ref(&fill)).unwrap();
        let inputs = body["inputs"].as_array().unwrap();
        assert_eq!(inputs[0]["forge"]["commit"], "HEAD");
        assert_eq!(
            inputs[4],
            json!({ "path": "/fleet/fills/0.pack", "files": { "path": format!("/shared/fleet/packs/core/{}.pack", "ab".repeat(20)) } })
        );
        let script = body["command"][2].as_str().unwrap();
        let index = script.find("index-pack --stdin --fix-thin < '/fleet/fills/0.pack'");
        let checkout = script.find(&format!("checkout --quiet '{}'", "ab".repeat(20)));
        let command = script.find("exec \"$@\"");
        assert!(index < checkout && checkout < command, "{script}");
        assert!(index.is_some(), "{script}");
    }

    #[test]
    fn the_outputs_are_cut_into_a_folder_of_the_gatherers_own_and_none_under_the_runs() {
        let run = tempfile::tempdir().unwrap();
        let out = run.path().join("out");
        let keep = out.join(".fleet");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::write(keep.join("log"), b"ran\n").unwrap();
        std::fs::write(out.join("hello.txt"), b"hello\n").unwrap();
        let own = tempfile::tempdir().unwrap();
        let scratch = own.path().join("fleet");
        let ran = std::process::Command::new("python3")
            .args(["-c", GATHER, "4"])
            .arg(&keep)
            .arg(&scratch)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let manifest: Value = serde_json::from_slice(&ran.stdout).unwrap();
        let paths = pieces_of(&manifest);
        assert_eq!(paths.len(), 3, "{manifest}");
        for path in &paths {
            assert!(path.starts_with(scratch.to_str().unwrap()), "{path}");
        }
        let left: Vec<_> = std::fs::read_dir(&keep)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            ["log"],
            "the run's own folder holds only what the run wrote"
        );
    }

    #[test]
    fn an_output_the_gatherer_cannot_read_is_refused_and_the_rest_is_gathered() {
        let run = tempfile::tempdir().unwrap();
        let out = run.path().join("out");
        let keep = out.join(".fleet");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(out.join("private")).unwrap();
        std::fs::write(keep.join("log"), b"ran\n").unwrap();
        std::fs::write(out.join("hello.txt"), b"hello\n").unwrap();
        std::fs::write(out.join("secret"), b"0600\n").unwrap();
        std::fs::write(
            out.join("commits"),
            b"core 0000000000000000000000000000000000000000 /work\n",
        )
        .unwrap();
        std::fs::write(out.join("private").join("inside"), b"0700\n").unwrap();
        let own = tempfile::tempdir().unwrap();
        let scratch = own.path().join("fleet");
        let denied = format!(
            "import builtins, os, sys\nopened, scanned = builtins.open, os.scandir\n\
             def refuse(path):\n    raise PermissionError(13, 'Permission denied', path)\n\
             builtins.open = lambda path, *a, **k: refuse(path) if str(path).endswith(('/secret', '/commits')) else opened(path, *a, **k)\n\
             os.scandir = lambda path='.': refuse(path) if str(path).endswith('/private') else scanned(path)\n\
             exec({GATHER:?})\n"
        );
        let ran = std::process::Command::new("python3")
            .args(["-c", &denied, "64"])
            .arg(&keep)
            .arg(&scratch)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let manifest: Value = serde_json::from_slice(&ran.stdout).unwrap();
        let names: Vec<&str> = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|file| file["name"].as_str())
            .collect();
        assert_eq!(names, ["log", "out/hello.txt"], "{manifest}");
        let refused = manifest["refused"].to_string();
        assert!(
            refused.contains("out/secret: not read: Permission denied"),
            "{refused}"
        );
        assert!(
            refused.contains("out/private: not read: Permission denied"),
            "{refused}"
        );
        assert!(
            refused.contains("out/commits: not read: Permission denied"),
            "{refused}"
        );
    }

    #[test]
    fn a_wait_beats_twice_in_the_egress_wait() {
        assert_eq!(every(1000).unwrap(), 32);
        assert_eq!(every(250).unwrap(), 8);
        assert!(every(31).is_err());
    }

    fn piece(n: u64, piece: Value) -> Value {
        json!({ "n": n, "piece": piece })
    }

    fn event(n: u64, text: &str) -> Value {
        piece(
            n,
            json!({ "event": { "seq": n, "data": hex::encode(text) } }),
        )
    }

    fn answer(n: u64, status: u16, body: &[u8]) -> Value {
        let response = http::Response {
            status,
            headers: Vec::new(),
            body: body.to_vec(),
        };
        piece(n, json!({ "answer": hex::encode(abi::encode(&response)) }))
    }

    #[test]
    fn a_calls_pieces_settle_at_its_answer_or_its_failure_and_not_before() {
        let alive = "event: alive\ndata: {}\n\n";
        assert_eq!(settled(&[event(0, alive)]).unwrap(), None);
        let answered = settled(&[event(0, alive), answer(1, 201, b"{}")]).unwrap();
        assert_eq!(
            answered,
            Some(Reply::Answered {
                status: 201,
                body: b"{}".to_vec(),
                events: vec![alive.as_bytes().to_vec()],
            })
        );
        let failed = settled(&[piece(0, json!({ "failed": "the stream broke off" }))]).unwrap();
        assert_eq!(
            failed,
            Some(Reply::Failed {
                why: "the stream broke off".into(),
                events: Vec::new(),
            })
        );
        assert!(settled(&[piece(0, json!({ "other": 1 }))]).is_err());
    }

    #[test]
    fn an_ended_event_reads_whole_even_when_the_stream_cut_it_in_two() {
        let events = vec![
            b"event: alive\ndata: {}\n\nevent: end".to_vec(),
            b"ed\ndata: {\"line\": \"pod p ended: exit code 0\", \"exit_code\": 0, \"pod\": null}\n\n"
                .to_vec(),
        ];
        let said = sse(&events);
        assert_eq!(said.len(), 2);
        assert_eq!(said[0].0, "alive");
        let ended = ended_event(&events).unwrap();
        assert_eq!(ended["exit_code"], 0);
    }

    fn view(run: Option<(&str, Value)>, fetch: Option<(&str, Value)>) -> Value {
        let status = |held: Option<(&str, Value)>| {
            held.map_or(
                Value::Null,
                |(state, code)| json!({ "state": state, "exit_code": code }),
            )
        };
        json!({
            "name": "duckpond-41",
            "phase": "Running",
            "reason": null,
            "containers": [{ "name": "run", "status": status(run) }, { "name": "out", "status": null }],
            "init": [{ "name": "fetch", "status": status(fetch) }],
        })
    }

    #[test]
    fn an_end_reads_as_passed_failed_or_errored_and_says_when_the_fetch_failed() {
        let passed = ended_view(&view(Some(("terminated", json!(0))), None)).unwrap();
        assert_eq!(ended(&passed).state, "passed");
        let failed = ended_view(&view(Some(("terminated", json!(3))), None)).unwrap();
        let end = ended(&failed);
        assert_eq!((end.state, end.fetch_failed), ("failed", false));
        assert!(end.summary.contains("exit code 3"), "{}", end.summary);
        let unfetched = ended_view(&view(None, Some(("terminated", json!(1))))).unwrap();
        let end = ended(&unfetched);
        assert_eq!((end.state, end.fetch_failed), ("errored", true));
        assert_eq!(
            ended_view(&view(Some(("running", Value::Null)), None)),
            None
        );
        let gone = json!({ "line": "pod duckpond-41 is gone", "exit_code": null, "pod": null });
        assert_eq!(ended(&gone).state, "errored");
    }

    #[test]
    fn a_call_names_the_service_its_run_its_number_and_a_json_body_by_its_part() {
        let asked = Asked {
            service: "pods",
            run: 7,
            attempt: 2,
            call: 3,
        };
        let part = json!({ "sha256": "11".repeat(32) });
        let op = call_op(
            &asked,
            http::Method::Post,
            "/v1/pods",
            vec![part.clone()],
            false,
        );
        assert_eq!(
            op,
            json!({ "call": {
                "run": 7, "attempt": 2, "call": 3,
                "request": { "http": {
                    "service": "pods",
                    "head": { "method": "post", "target": "/v1/pods", "headers": [["content-type", "application/json"]] },
                    "body": [part],
                }},
                "stream": false, "wait": null,
            }})
        );
        let wait = call_op(
            &asked,
            http::Method::Get,
            "/v1/pods/p/wait?every=32",
            Vec::new(),
            true,
        );
        assert_eq!(
            wait["call"]["request"]["http"]["head"]["headers"],
            json!([])
        );
        assert_eq!(wait["call"]["stream"], true);
    }

    #[test]
    fn gathered_outputs_become_files_changes_under_the_folder_and_packs_at_their_path() {
        let commit = "ab".repeat(20);
        let manifest = json!({
            "files": [
                { "name": "log", "size": 3, "pieces": ["/out/.fleet/pieces/0-0"] },
                { "name": "out/a/b", "size": 0, "pieces": [] },
            ],
            "packs": [{ "repo": "core", "commit": commit, "size": 5, "pieces": ["/out/.fleet/pieces/2-0"] }],
            "refused": ["out/commits line \"x\" is no `<repo> <commit> <path>`"],
        });
        let blob = |n: &str| format!("sha256:{}", n.repeat(32));
        let gathered = Gathered {
            manifest,
            blobs: vec![
                json!({ "path": "/out/.fleet/pieces/0-0", "blob": blob("01"), "size": 3 }),
                json!({ "path": "/out/.fleet/pieces/2-0", "blob": blob("02"), "size": 5 }),
            ],
        };
        let (made, refused) = changes(&gathered, "/shared/fleet/7/").unwrap();
        let paths: Vec<&str> = made
            .iter()
            .map(|change| change["put"]["path"].as_str().unwrap())
            .collect();
        let pack = format!("/shared/fleet/packs/core/{commit}.pack");
        assert_eq!(
            paths,
            [
                "/shared/fleet/7/log",
                "/shared/fleet/7/out/a/b",
                pack.as_str()
            ]
        );
        assert_eq!(
            made[0]["put"]["content"],
            json!({ "chunks": { "size": 3, "chunks": [{ "sha256": "01".repeat(32) }] } })
        );
        assert_eq!(made[1]["put"]["content"]["chunks"]["chunks"], json!([]));
        assert_eq!(refused.len(), 1);
        let unput = Gathered {
            blobs: Vec::new(),
            ..gathered
        };
        assert!(changes(&unput, "/f/").unwrap_err().contains("was not put"));
    }

    struct Fake {
        asked: Vec<(http::Method, String, Option<Value>)>,
        answers: VecDeque<Option<Reply>>,
        packs: Vec<(String, String)>,
        stopper: Option<oneshot::Sender<Ending>>,
    }

    impl Fake {
        fn answering(answers: Vec<Option<Reply>>) -> Fake {
            Fake {
                asked: Vec::new(),
                answers: answers.into(),
                packs: Vec::new(),
                stopper: None,
            }
        }

        fn routes(&self) -> Vec<String> {
            self.asked
                .iter()
                .map(|(method, target, _)| format!("{} {target}", method.as_str()))
                .collect()
        }
    }

    impl Relay for Fake {
        async fn call(
            &mut self,
            method: http::Method,
            target: String,
            body: Option<Value>,
            _stream: bool,
        ) -> Result<Reply, String> {
            self.asked.push((method, target, body));
            match self.answers.pop_front() {
                Some(Some(reply)) => Ok(reply),
                Some(None) => {
                    if let Some(stopper) = self.stopper.take() {
                        let _ = stopper.send(Ending::Stopped);
                    }
                    std::future::pending().await
                }
                None => Err("the fake holds no more answers".into()),
            }
        }

        async fn pack(&mut self, repo: &str, commit: &str) -> Result<bool, String> {
            Ok(self
                .packs
                .iter()
                .any(|held| held.0 == repo && held.1 == commit))
        }
    }

    fn json_reply(status: u16, body: Value) -> Option<Reply> {
        Some(Reply::Answered {
            status,
            body: serde_json::to_vec(&body).unwrap(),
            events: Vec::new(),
        })
    }

    fn created() -> Option<Reply> {
        json_reply(201, json!({ "pod": { "name": "duckpond-41" } }))
    }

    fn ending(data: Value) -> Option<Reply> {
        let alive = b"event: alive\ndata: {}\n\n".to_vec();
        let ended = format!("event: ended\ndata: {data}\n\n").into_bytes();
        Some(Reply::Answered {
            status: 200,
            body: Vec::new(),
            events: vec![alive, ended],
        })
    }

    fn exec(manifest: Value) -> Option<Reply> {
        json_reply(
            200,
            json!({ "exit_code": 0, "stdout": manifest.to_string(), "stderr": "" }),
        )
    }

    fn deleted() -> Option<Reply> {
        json_reply(200, json!({ "deleted": "duckpond-41" }))
    }

    struct Woken(std::sync::atomic::AtomicBool);

    impl std::task::Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn driven<F: Future>(future: F) -> F::Output {
        let woken = Arc::new(Woken(std::sync::atomic::AtomicBool::new(false)));
        let waker = std::task::Waker::from(woken.clone());
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
            let woke = woken.0.swap(false, Ordering::SeqCst);
            assert!(woke, "the flow waits on a call nothing will answer");
        }
    }

    fn flowed(
        fake: &mut Fake,
        workload: &Workload,
        stop: oneshot::Receiver<Ending>,
    ) -> (Flowed, Vec<String>) {
        let mut pods = Vec::new();
        let mut started = |pod: &str| pods.push(pod.to_owned());
        let flowed = driven(flow(fake, workload, 32, stop, &mut started));
        (flowed, pods)
    }

    #[test]
    fn a_pod_is_made_waited_for_its_outputs_put_in_one_bundle_and_deleted() {
        let manifest = json!({
            "files": [{ "name": "log", "size": 2, "pieces": ["/out/.fleet/pieces/0-0"] }],
            "packs": [],
            "refused": [],
        });
        let blobs = json!({ "blobs": [{ "path": "/out/.fleet/pieces/0-0", "blob": format!("sha256:{}", "01".repeat(32)), "size": 2 }], "kind": "chunk" });
        let mut fake = Fake::answering(vec![
            created(),
            ending(
                json!({ "line": "pod duckpond-41 ended: exit code 0 (Completed)", "exit_code": 0, "pod": null }),
            ),
            exec(manifest.clone()),
            json_reply(200, blobs.clone()),
            deleted(),
        ]);
        let (_stopper, stop) = oneshot::channel();
        let (flowed, pods) = flowed(&mut fake, &workload(), stop);
        assert_eq!(
            fake.routes(),
            [
                "POST /v1/pods",
                "GET /v1/pods/duckpond-41/wait?every=32",
                "POST /v1/pods/duckpond-41/exec",
                "POST /v1/pods/duckpond-41/blob",
                "DELETE /v1/pods/duckpond-41",
            ]
        );
        assert_eq!(pods, ["duckpond-41"]);
        let put = fake.asked[3].2.clone().unwrap();
        assert_eq!(
            put,
            json!({ "paths": ["/out/.fleet/pieces/0-0"], "kind": "chunk", "container": "out" })
        );
        let exec = fake.asked[2].2.clone().unwrap();
        assert_eq!(exec["container"], "out");
        assert_eq!(exec["command"][3], json!(out::CHUNK_SIZE.to_string()));
        assert_eq!(
            flowed,
            Flowed::Ended {
                state: "passed",
                summary: "pod duckpond-41 ended: exit code 0 (Completed)".into(),
                gathered: Some(Gathered {
                    manifest,
                    blobs: blobs["blobs"].as_array().unwrap().clone(),
                }),
            }
        );
    }

    #[test]
    fn a_wait_cut_before_the_end_reads_the_pod_and_opens_another() {
        let running = json!({ "pod": view(Some(("running", Value::Null)), None) });
        let empty = json!({ "files": [], "packs": [], "refused": [] });
        let mut fake = Fake::answering(vec![
            created(),
            Some(Reply::Failed {
                why: "the stream broke off".into(),
                events: vec![b"event: alive\ndata: {}\n\n".to_vec()],
            }),
            json_reply(200, running),
            ending(
                json!({ "line": "pod duckpond-41 ended: exit code 3 (Error)", "exit_code": 3, "pod": null }),
            ),
            exec(empty),
            deleted(),
        ]);
        let (_stopper, stop) = oneshot::channel();
        let (flowed, _) = flowed(&mut fake, &workload(), stop);
        assert_eq!(
            fake.routes(),
            [
                "POST /v1/pods",
                "GET /v1/pods/duckpond-41/wait?every=32",
                "GET /v1/pods/duckpond-41",
                "GET /v1/pods/duckpond-41/wait?every=32",
                "POST /v1/pods/duckpond-41/exec",
                "DELETE /v1/pods/duckpond-41",
            ]
        );
        let Flowed::Ended { state, .. } = flowed else {
            panic!("not ended: {flowed:?}");
        };
        assert_eq!(state, "failed");
    }

    #[test]
    fn a_stopped_run_deletes_its_pod_and_gathers_nothing() {
        let mut fake = Fake::answering(vec![created(), None, deleted()]);
        let (stopper, stop) = oneshot::channel();
        fake.stopper = Some(stopper);
        let (flowed, _) = flowed(&mut fake, &workload(), stop);
        assert_eq!(flowed, Flowed::Stopped(Ending::Stopped));
        assert_eq!(
            fake.routes(),
            [
                "POST /v1/pods",
                "GET /v1/pods/duckpond-41/wait?every=32",
                "DELETE /v1/pods/duckpond-41",
            ]
        );
    }

    #[test]
    fn a_pod_the_service_refuses_is_reported_in_its_words_and_nothing_follows() {
        let refused = json_reply(
            403,
            json!({ "error": "pods \"duckpond-41\" is forbidden: exceeded quota: hardcap" }),
        );
        let mut fake = Fake::answering(vec![refused]);
        let (_stopper, stop) = oneshot::channel();
        let (flowed, pods) = flowed(&mut fake, &workload(), stop);
        assert_eq!(
            flowed,
            Flowed::Unmade(
                "the pod's creation answered 403: pods \"duckpond-41\" is forbidden: exceeded quota: hardcap"
                    .into()
            )
        );
        assert_eq!(fake.routes(), ["POST /v1/pods"]);
        assert!(pods.is_empty());
    }

    #[test]
    fn a_forge_input_forge_lacks_is_filled_from_its_pack_in_a_second_pod_once() {
        let commit = "ab".repeat(20);
        let unfetched = json!({ "line": "pod duckpond-41 ended: fetching its inputs failed, exit code 1", "exit_code": null, "pod": view(None, Some(("terminated", json!(1)))) });
        let empty = json!({ "files": [], "packs": [], "refused": [] });
        let mut fake = Fake::answering(vec![
            created(),
            ending(unfetched.clone()),
            deleted(),
            created(),
            ending(unfetched),
            exec(empty),
            deleted(),
        ]);
        fake.packs.push(("core".into(), commit.clone()));
        let (_stopper, stop) = oneshot::channel();
        let (flowed, pods) = flowed(&mut fake, &workload(), stop);
        assert_eq!(pods.len(), 2);
        let again = fake.asked[3].2.clone().unwrap();
        assert_eq!(again["inputs"][0]["forge"]["commit"], "HEAD");
        assert_eq!(
            again["inputs"][4]["files"]["path"],
            json!(format!("/shared/fleet/packs/core/{commit}.pack"))
        );
        assert_eq!(fake.asked.len(), 7);
        let Flowed::Ended { state, summary, .. } = flowed else {
            panic!("not ended: {flowed:?}");
        };
        assert_eq!(state, "errored");
        assert!(summary.contains("fetching its inputs failed"), "{summary}");
    }
}
