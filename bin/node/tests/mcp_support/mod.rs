//! In-process MCP read/query fixture with real identity, programmable account,
//! model configuration, source attribution and signed seeding writes.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write as _};
use std::process::{Child, Command, Stdio};

use commonware_cryptography::{Signer as _, ed25519};
use host::Host;
use noded::testkit::{InProcDaemon, committed_module};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

#[path = "../../src/wire.rs"]
mod wire;
use wire::{agent, chat, runs};

pub const AGENT_ID: &str = "quackbot";
/// this fixture's identity chain id — and the value the `runs` guest reads out
/// of its genesis `__config` record.
const CHAIN_ID: &str = "mcp-test";
/// Deterministic Ed25519 fixture seed; every admitted write is actually signed.
pub const OWNER: u64 = 7;
static FRAME_SEQ: AtomicU64 = AtomicU64::new(1);
pub fn owner_key() -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(OWNER)
}

/// the in-memory authenticated store every store-backed module in this fixture
/// rides — native and guest alike.
fn mem_store() -> Box<dyn sdk::MerkleStore> {
    Box::new(sdk_testkit::MemStore::new())
}

pub struct Harness {
    // dropped BEFORE `dir` (fields drop in declaration order): the daemon's Drop
    // joins the actor, closing the host's forge/qmdb handles, so the tempdir is
    // removed only afterward.
    daemon: InProcDaemon,
    dir: tempfile::TempDir,
}

impl Harness {
    /// stand the node up and register `AGENT_ID` as the model every assertion
    /// in the calling test is written against.
    pub fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ducktape mcp-e2e")
            .tempdir()
            .expect("harness tempdir");
        let forge_base = dir.path().join("forge");
        // ONE blob store, shared by the http surface and the forge module.
        //
        // Forge materializes a pushed packfile out of the blob store, so a test
        // that pushes real git objects must upload them where the module will
        // look. With the in-memory default these are two disconnected stores and
        // the objects are simply absent — the PR-diff read then fails with
        // "objects ... are not fully materialized", which reads like a forge bug
        // and is really a harness one.
        let blob_root = dir.path().join("blobs");
        let blobs = noded::blobs::BlobHandle::persistent(&blob_root).expect("harness blob store");

        let daemon = InProcDaemon::start_with_blob_root(
            // agent, chat, tasks and runs are the COMMITTED guests this repo
            // ships (`crates/modules/apps/<id>/component.wasm`): their source
            // lives in ducktape-modules, and the sibling wiring a native
            // constructor used to take as arguments is compiled into each
            // guest. The system modules, forge and files are core-resident and
            // still native. `block_on` because a guest comes up asynchronously
            // and the testkit builds its host on the actor thread, before that
            // thread's own executor starts.
            move || {
                futures::executor::block_on(async {
                    Host::genesis(vec![
                        // no commonware context in this closure, so every
                        // store-backed module rides the in-memory test double.
                        Box::new(committed_module("agent", mem_store(), CHAIN_ID).await),
                        Box::new(identity::Identity::new(
                            "identity",
                            mem_store(),
                            CHAIN_ID.into(),
                        )),
                        Box::new(committed_module("chat", mem_store(), CHAIN_ID).await),
                        Box::new(saga::SagaModule::new("saga", mem_store())),
                        Box::new(committed_module("tasks", mem_store(), CHAIN_ID).await),
                        Box::new(dispatch::DispatchModule::new(
                            "dispatch",
                            "saga",
                            "identity",
                            mem_store(),
                        )),
                        Box::new(
                            attribution::AttributionModule::new("attribution", mem_store())
                                .with_subscribers(["agent"]),
                        ),
                        Box::new(
                            forge::Forge::with_blobs("forge", forge_base, blobs)
                                .expect("forge module"),
                        ),
                        // duckfs, the shared filesystem the read plane pages
                        // through. in-memory like every other double here: the
                        // module's odb is not what these tests are about.
                        Box::new(files::Files::in_mem()),
                        Box::new(committed_module("runs", mem_store(), CHAIN_ID).await),
                    ])
                    .expect("genesis")
                })
            },
            [
                "identity",
                "agent",
                "attribution",
                "runs",
                "chat",
                "tasks",
                "saga",
                "dispatch",
                "forge",
                "files",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            Some(blob_root),
        );

        let harness = Harness { daemon, dir };
        let created = harness.submit(
            "identity",
            serde_json::to_value(identity::IdentityMsg::Create {
                name: "Alice".into(),
                scheme: identity::KeyScheme::Ed25519,
            })
            .unwrap(),
            OWNER,
        );
        assert!(
            created.get("height").is_some(),
            "identity creation: {created}"
        );
        harness.register_model(AGENT_ID, "Quackbot");
        harness
    }

    pub fn node_url(&self) -> String {
        self.daemon.node_url()
    }

    /// duckfs's own client against this node, signing as the harness owner —
    /// the lane a real member commits through. the read tools then page over
    /// what consensus holds, never over something this fixture handed them.
    pub fn files(&self) -> duckfs_client::http::HttpNode {
        let signer = owner_key();
        duckfs_client::http::HttpNode::new(self.node_url()).with_frame_signer(std::sync::Arc::new(
            move |target, payload| {
                node::encode_frame(
                    &signer,
                    FRAME_SEQ.fetch_add(1, Ordering::Relaxed),
                    &sdk::Msg {
                        target: target.into(),
                        payload,
                    },
                )
            },
        ))
    }

    pub fn register_model(&self, id: &str, name: &str) -> u64 {
        let provisioned = self.submit(
            "agent",
            serde_json::to_value(agent::AgentMsg::Provision {
                request_id: id.into(),
                name: name.into(),
                program: runs::model_program(id),
            })
            .unwrap(),
            OWNER,
        );
        assert!(
            provisioned.get("height").is_some(),
            "provision: {provisioned}"
        );
        let reply = self.query(
            "identity",
            serde_json::to_value(identity::IdentityQuery::All {
                from: 0,
                limit: identity::MAX_QUERY_LIMIT,
            })
            .unwrap(),
        );
        let identity::IdentityReply::Accounts(accounts) =
            serde_json::from_value(reply).expect("identity reply")
        else {
            panic!("account list");
        };
        let account = accounts
            .into_iter()
            .find(|account| {
                account.name == name && matches!(account.control, identity::Control::Program { .. })
            })
            .expect("provisioned account")
            .number;
        let registered = self.submit(
            "runs",
            serde_json::to_value(runs::RunsMsg::ConfigureModel {
                operation: runs::ModelMsg::RegisterModel {
                    account,
                    agent_id: id.into(),
                    display_name: name.into(),
                    capability: "codex".into(),
                    recipe_hash: None,
                    skills: None,
                },
            })
            .unwrap(),
            OWNER,
        );
        assert!(
            registered.get("height").is_some(),
            "register model: {registered}"
        );
        account
    }

    /// Start actual consensus work without a provider accepting it. The read
    /// tools can then prove their live-run binding without an invented run id.
    pub fn pending_run(&self) -> String {
        for message in [
            chat::ChatMsg::CreateChannel {
                channel_id: "mcp-read".into(),
                name: "MCP read".into(),
                post_policy: chat::PostPolicy::Open,
            },
            chat::ChatMsg::PostMessage {
                channel_id: "mcp-read".into(),
                message_id: "anchor".into(),
                blocks: vec![chat::Block::paragraph("read the current record")],
                thread: None,
            },
        ] {
            let reply = self.submit("chat", serde_json::to_value(message).unwrap(), OWNER);
            assert!(reply.get("height").is_some(), "chat fixture: {reply}");
        }
        let reply = self.submit(
            "runs",
            serde_json::to_value(runs::RunsMsg::RequestRun {
                agent_id: AGENT_ID.into(),
                channel_id: "mcp-read".into(),
                anchor_seq: 1,
                demands: Default::default(),
                skills: Vec::new(),
            })
            .unwrap(),
            OWNER,
        );
        assert!(reply.get("height").is_some(), "run fixture: {reply}");
        let settled = self
            .daemon
            .drain_ready_work()
            .expect("commit queued program work");
        assert!(settled.height > reply["height"].as_u64().expect("request height"));
        let reply = self.query(
            "runs",
            serde_json::to_value(runs::RunsQuery::PendingRuns).unwrap(),
        );
        let runs::RunsReply::PendingRuns(runs) =
            serde_json::from_value(reply).expect("pending run reply")
        else {
            panic!("pending runs");
        };
        runs.into_iter()
            .find(|run| run.agent_id == AGENT_ID)
            .expect("the actual pending run")
            .run_id
    }

    /// a `ducktape mcp` subprocess wired exactly as the node's provisioner wires
    /// one: the node url and the agent id in the environment, and NOTHING else.
    /// if the binary needed more than this to work, the provisioner would have
    /// to supply more than this — so the test wires only what production does.
    pub fn mcp(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("mcp");
        cmd.env("DUCKTAPE_NODE", self.node_url())
            .env("DUCKTAPE_RUN_AGENT", AGENT_ID)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd
    }

    /// A write-capable MCP shape with a scoped endpoint that is deliberately
    /// unavailable. Production creates this endpoint inside the provisioner;
    /// this read/query harness does not dispatch runs.
    pub fn mcp_with_action(&self, run_id: &str) -> Command {
        let mut cmd = self.mcp();
        cmd.env(
            "DUCKTAPE_RUN_ACTION_URL",
            "http://127.0.0.1:9/v1/run-action",
        )
        .env(
            "DUCKTAPE_RUN_ACTION_TOKEN",
            "abababababababababababababababababababababababababababababababab",
        )
        .env("DUCKTAPE_RUN_ID", run_id);
        cmd
    }

    /// a `ducktape mcp` subprocess with NO agent bound — the "started outside a
    /// provisioned run" shape.
    pub fn mcp_agentless(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ducktape"));
        cmd.arg("mcp");
        cmd.env("DUCKTAPE_NODE", self.node_url())
            .env_remove("DUCKTAPE_RUN_AGENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        cmd
    }

    /// drive one `tools/call` through a real subprocess over stdio and return
    /// its `result`. the whole point of doing it this way rather than calling
    /// the handler in-process: it proves the FRAMING works — that a runner
    /// speaking MCP at this binary gets an answer back.
    pub fn call(&self, cmd: Command, tool: &str, arguments: Value) -> Value {
        self.session(cmd, &[json!({"name": tool, "arguments": arguments})])
            .pop()
            .expect("one call, one result")
    }

    /// a full MCP session: initialize, then one `tools/call` per entry, in
    /// order, down one stdin — the shape a real runner uses.
    pub fn session(&self, mut cmd: Command, calls: &[Value]) -> Vec<Value> {
        let mut child: Child = cmd.spawn().expect("spawn ducktape mcp");
        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));

        let mut frames = vec![
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}}).to_string(),
            // the notification a real client always sends — and which the server
            // must NOT answer. if it does, every id below reads off by one and
            // the test fails loudly.
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        ];
        for (i, params) in calls.iter().enumerate() {
            frames.push(
                json!({
                    "jsonrpc": "2.0",
                    "id": i + 1,
                    "method": "tools/call",
                    "params": params,
                })
                .to_string(),
            );
        }
        for frame in &frames {
            writeln!(stdin, "{frame}").expect("write frame");
        }
        stdin.flush().expect("flush");
        drop(stdin);

        let mut responses: Vec<Value> = Vec::new();
        for line in stdout.lines() {
            let line = line.expect("read a response line");
            if line.trim().is_empty() {
                continue;
            }
            responses.push(serde_json::from_str(&line).expect("a response is json"));
        }
        let _ = child.wait();

        assert_eq!(
            responses.len(),
            calls.len() + 1,
            "expected one initialize response plus one per call — a notification that got \
             answered would show up right here: {responses:#?}"
        );
        assert_eq!(responses[0]["id"], 0, "the first response is initialize's");
        responses
            .into_iter()
            .skip(1)
            .map(|r| r["result"].clone())
            .collect()
    }

    /// the `initialize` result, for the guide/protocol assertions.
    pub fn initialize(&self) -> Value {
        let mut child = self.mcp().spawn().expect("spawn ducktape mcp");
        let mut stdin = child.stdin.take().expect("stdin");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}})
        )
        .expect("write initialize");
        stdin.flush().expect("flush");
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read initialize reply");
        drop(stdin);
        let _ = child.wait();
        let value: Value = serde_json::from_str(&line).expect("json");
        value["result"].clone()
    }

    /// submit an op straight at the node — the test's own seeding lane, and the
    /// oracle every write assertion is checked against.
    pub fn submit(&self, target: &str, payload: Value, seed: u64) -> Value {
        let signer = ed25519::PrivateKey::from_seed(seed);
        let frame = node::encode_frame(
            &signer,
            FRAME_SEQ.fetch_add(1, Ordering::Relaxed),
            &sdk::Msg {
                target: target.into(),
                payload: sdk::wire::encode(&payload),
            },
        );
        let (_status, raw) = nettest::try_http_bytes_with(
            self.daemon.port(),
            "POST",
            "/v1/submit/frame",
            "application/octet-stream",
            &[],
            &frame,
        )
        .expect("signed submit");
        serde_json::from_slice(&raw).expect("submit reply")
    }

    /// query the node directly — how a test reads back what the MCP server
    /// wrote, WITHOUT going through the MCP server that claims to have written
    /// it.
    pub fn query(&self, target: &str, query: Value) -> Value {
        self.post("/v1/query", &json!({"target": target, "query": query}))
    }

    /// Land raw bytes in the node's blob store; returns the digest hex.
    ///
    /// This is the production path too: the smart-HTTP bridge uploads a
    /// packfile and `PushRefs` then names its digest. Consensus records only
    /// `ref -> oid` — the OBJECTS stay node-local — which is why a test that
    /// wants a computable PR diff has to put real ones here rather than
    /// fabricating an oid.
    pub fn put_blob(&self, bytes: &[u8]) -> String {
        let (name, token) = self.daemon.write_header();
        let (status, body) = nettest::try_http_bytes_with(
            self.daemon.port(),
            "POST",
            "/v1/files/blob",
            "application/octet-stream",
            &[(name, token)],
            bytes,
        )
        .expect("node reachable");
        assert_eq!(
            status,
            200,
            "blob upload failed: {}",
            String::from_utf8_lossy(&body)
        );
        let receipt: Value = serde_json::from_slice(&body).expect("blob receipt json");
        receipt["digest"]
            .as_str()
            .expect("blob receipt names a digest")
            .to_string()
    }

    /// Every seeding POST carries the node's operator credential: the harness
    /// owns this node, and a mutating route refuses a caller that holds neither
    /// that nor a user signature.
    fn post(&self, path: &str, body: &Value) -> Value {
        let (name, token) = self.daemon.write_header();
        let bytes = serde_json::to_vec(body).expect("request body serializes");
        let (_status, raw) = nettest::try_http_bytes_with(
            self.daemon.port(),
            "POST",
            path,
            "application/json",
            &[(name, token)],
            &bytes,
        )
        .expect("node reachable");
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    }
}

/// the text a `tools/call` result carries, and whether it was a refusal.
pub fn content(result: &Value) -> (bool, String) {
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    (is_error, text)
}

/// a successful tool result's json payload, parsed back out of the content text.
pub fn payload(result: &Value) -> Value {
    let (is_error, text) = content(result);
    assert!(!is_error, "expected a success, got a refusal: {text}");
    serde_json::from_str(&text).expect("a successful tool result carries json")
}
