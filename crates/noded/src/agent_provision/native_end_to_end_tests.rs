//! One connected native Host -> Chief/Pi -> independent worker/Pi -> Jobs ->
//! Chief/Pi regression. Only the vendor's HTTP SSE decisions are scripted.
//! The harness owns block scheduling and oracle delivery, not module replies.
//! Requires an installed Pi and a current `ducktape` MCP executable; opt-in so
//! ordinary noded unit tests do not silently depend on those external binaries.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use commonware_cryptography::ed25519;
use host::{BlockContext, Host};
use sdk::{Msg, Origin};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{collections::VecDeque, future::Future, path::PathBuf, process::Stdio};
use tokio::io::AsyncWriteExt as _;

#[allow(dead_code)]
#[path = "../../../../bin/node/src/chief_cli/plan.rs"]
mod plan;

const TASK: &str = "connected-task";
const JOB: &str = "connected-dispatch";
const REPORT: &str = "Connected worker evidence: native Pi published this authenticated report. 🦆";
const CHECKPOINT: &str = "{\"summary\":\"Independent worker still running\",\"next\":\"Finish evidence review\",\"artifacts\":[]}";

fn signer(seed: u64) -> ed25519::PrivateKey {
    ed25519::PrivateKey::from_seed(seed)
}
fn key(seed: u64) -> Vec<u8> {
    signer(seed).public_key().to_vec()
}
fn message(target: &str, value: impl serde::Serialize) -> Msg {
    Msg {
        target: target.into(),
        payload: sdk::wire::encode(&value),
    }
}
fn at(height: u64, origin: Origin) -> BlockContext {
    BlockContext {
        height,
        consensus_time: height,
        origin,
    }
}
fn store() -> Box<dyn sdk::MerkleStore> {
    Box::new(sdk_testkit::MemStore::new())
}

struct Network {
    host: Host,
    height: u64,
    commands: futures::channel::mpsc::Receiver<crate::NodeCommand>,
    hub: crate::stream::StreamHub,
    link: NodeLink,
    requests: VecDeque<saga::WorkerRequest>,
    trace: Vec<host::DispatchRecord>,
    server: tokio::task::JoinHandle<()>,
    audit: PathBuf,
}
impl Drop for Network {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Network {
    async fn new(root: &Path) -> Self {
        let mut validators = valset::Valset::new("valset", store(), "governance");
        validators.seed(key(8)).await.unwrap();
        validators.finish_seed().await.unwrap();
        let host = Host::genesis(vec![
            Box::new(identity::Identity::new(
                "identity",
                store(),
                "connected-chief".into(),
            )),
            Box::new(
                attribution::AttributionModule::new("attribution", store())
                    .with_subscribers(["agent"]),
            ),
            Box::new(agent::AgentModule::new(
                "agent",
                store(),
                agent::Siblings {
                    identity: "identity".into(),
                    attribution: "attribution".into(),
                    dispatch: "dispatch".into(),
                },
            )),
            Box::new(
                chat::Chat::new("chat", store())
                    .with_identity("identity")
                    .with_attribution("attribution"),
            ),
            Box::new(
                pages::Pages::new("pages", store())
                    .with_identity("identity")
                    .with_attribution("attribution")
                    .with_files("files"),
            ),
            Box::new(validators),
            Box::new(capability::CapabilityRegistry::new(
                "capability",
                store(),
                Some("valset".into()),
            )),
            Box::new(saga::SagaModule::with_assignment(
                "saga",
                store(),
                "valset",
                "capability",
                saga::LeasePolicy::Open,
            )),
            Box::new(dispatch::DispatchModule::new(
                "dispatch",
                "saga",
                "identity",
                store(),
            )),
            Box::new(tasks::Tasks::new(
                "tasks",
                "identity",
                "attribution",
                store(),
            )),
            Box::new(files::Files::open("files", root.join("files")).unwrap()),
            Box::new(
                runs::RunsModule::new(
                    "runs",
                    "chat",
                    "saga",
                    "attribution",
                    "dispatch",
                    "agent",
                    Some("tasks".into()),
                    Some("tasks".into()),
                )
                .with_pages_module("pages")
                .with_files_module("files")
                .with_time_unit(sdk::genesis_config::TimeUnit::Height),
            ),
        ])
        .unwrap();
        let (handle, commands, hub) = crate::NodeHandle::channel();
        let workspace = root.join("operator");
        std::fs::create_dir_all(&workspace).unwrap();
        let token = crate::admin::mint_operator_token(&workspace).unwrap();
        let handle = handle.with_admin(crate::AdminConfig {
            operator_token: Some(token),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                crate::router(handle).into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            host,
            height: 0,
            commands,
            hub,
            requests: VecDeque::new(),
            trace: vec![],
            server,
            audit: root.join("host-requests.jsonl"),
            link: NodeLink::new(format!("http://{address}")).with_workspace_credential(&workspace),
        }
    }
    fn events(&mut self, events: Vec<sdk::Event>) {
        self.requests.extend(
            events
                .into_iter()
                .filter_map(|event| saga::decode_worker_request(&event.payload).ok()),
        );
    }
    async fn commit(
        &mut self,
        origin: Origin,
        msg: Msg,
    ) -> Result<crate::BlockSummary, crate::Refused> {
        self.height += 1;
        let outcome = self
            .host
            .submit_at(at(self.height, origin), msg)
            .await
            .map_err(|error| crate::Refused::of_submit(&error))?;
        self.events(outcome.events);
        self.trace.extend(outcome.dispatches);
        self.drain().await;
        let root_hash = duckfs_core::to_hex(&self.host.root_hash().0);
        self.hub.publish_block(
            self.height,
            root_hash.clone(),
            crate::stream::BlockWake::TipOnly,
        );
        Ok(crate::BlockSummary {
            height: self.height,
            root_hash,
        })
    }
    async fn drain(&mut self) {
        // Queue exhaustion is a committed event barrier, never sleep/retry.
        while self.host.has_pending_work().await.unwrap() {
            self.height += 1;
            let outcome = self
                .host
                .submit_block(at(self.height, Origin::System), vec![])
                .await
                .unwrap();
            self.trace.extend(outcome.internal_dispatches());
            self.events(outcome.events);
        }
    }
    async fn submit(&mut self, seed: u64, target: &str, value: impl serde::Serialize) {
        let frame = node::encode_frame(&signer(seed), self.height + 1, &message(target, value));
        let (origin, msg) = node::decode_frame(&frame).expect("real signed frame");
        self.commit(origin, msg).await.unwrap();
    }
    async fn command(&mut self, command: crate::NodeCommand) {
        use std::io::Write as _;
        let entry = match &command {
            crate::NodeCommand::Query { target, req, .. }
            | crate::NodeCommand::QueryAs { target, req, .. } => {
                json!({"kind":"query", "target":target, "input":sdk::wire::decode::<Value>(req).ok()})
            }
            crate::NodeCommand::Submit {
                target, payload, ..
            } => {
                json!({"kind":"operator_submit", "target":target, "input":sdk::wire::decode::<Value>(payload).ok()})
            }
            crate::NodeCommand::SubmitFrame { frame, .. } => {
                let (origin, msg) = node::decode_frame(frame).unwrap();
                json!({"kind":"signed_submit", "origin":origin, "target":msg.target, "input":sdk::wire::decode::<Value>(&msg.payload).ok()})
            }
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit)
            .unwrap();
        writeln!(file, "{entry}").unwrap();
        match command {
            crate::NodeCommand::Query { target, req, reply } => {
                let result = self
                    .host
                    .query(&target, &req)
                    .await
                    .map_err(|error| crate::Refused::of(&error));
                let action_read = target == "runs"
                    && sdk::wire::decode::<Value>(&req)
                        .ok()
                        .is_some_and(|query| query.get("action_request").is_some());
                if action_read || result.is_err() {
                    let audited = result
                        .as_ref()
                        .map(|bytes| sdk::wire::decode::<Value>(bytes).ok())
                        .map_err(|refused| refused.message.clone());
                    writeln!(file, "{}", json!({"query_result":audited})).unwrap();
                }
                let _ = reply.send(result);
            }
            crate::NodeCommand::QueryAs {
                target,
                req,
                reader,
                reply,
            } => {
                let result = self
                    .host
                    .query_as(&target, &req, Origin::External(reader))
                    .await
                    .map_err(|error| crate::Refused::of(&error));
                let _ = reply.send(result);
            }
            crate::NodeCommand::SubmitFrame { frame, reply } => {
                let (origin, msg) = node::decode_frame(&frame).unwrap();
                let result = self.commit(origin, msg).await;
                if let Err(refused) = &result {
                    writeln!(
                        file,
                        "{}",
                        json!({"error":refused.message, "reason":refused.reason})
                    )
                    .unwrap();
                }
                let _ = reply.send(result);
            }
            crate::NodeCommand::Submit {
                target,
                payload,
                reply,
                ..
            } => {
                // The operator-credential route is this node's oracle/session
                // provisioner. Agent writes use verified SubmitFrame above.
                let _ = reply.send(
                    self.commit(Origin::External(key(8)), Msg { target, payload })
                        .await,
                );
            }
        }
    }
    async fn until<F: Future>(&mut self, future: F) -> F::Output {
        tokio::pin!(future);
        loop {
            tokio::select! {
                value = &mut future => return value,
                command = self.commands.next() => self.command(command.expect("live real Host actor")).await,
            }
        }
    }
    async fn query(&self, target: &str, query: impl serde::Serialize) -> Value {
        let bytes = self
            .host
            .query(target, &sdk::wire::encode(&query))
            .await
            .unwrap();
        sdk::wire::decode(&bytes).unwrap()
    }
    async fn conversation(&self, id: &str) -> runs::ConversationView {
        let value = self
            .query(
                "runs",
                runs::RunsQuery::Conversation {
                    conversation_id: id.into(),
                },
            )
            .await;
        serde_json::from_value(value["conversation"].clone()).unwrap()
    }
    async fn task(&self, plan: &plan::Plan) -> Value {
        let value = self
            .query(
                "pages",
                pages::PageQuery::Records {
                    page_id: plan.board_page_id.clone(),
                    after: None,
                    limit: 32,
                },
            )
            .await;
        let records = value["records"]["records"]
            .as_array()
            .expect("real Pages records");
        records
            .iter()
            .find(|row| row["data"]["value"]["id"] == TASK)
            .expect("Chief created canonical task")["data"]["value"]
            .clone()
    }
    async fn board_run(&self, plan: &plan::Plan) -> Value {
        // Decode this small fixture's single committed Pages chunk; there is
        // no parallel board in the harness and no mock response construction.
        let id = duckfs_core::to_hex(&Sha256::digest(
            serde_json::to_vec(&json!([plan.board_page_id, "run", JOB])).unwrap(),
        ));
        let row = self
            .query(
                "pages",
                pages::PageQuery::RecordState {
                    page_id: plan.board_page_id.clone(),
                    key: format!("chief-{id}-part-0"),
                },
            )
            .await;
        let chunk = &row["record_state"]["value"];
        assert_eq!(chunk["kind"], "run");
        assert_eq!(chunk["entityId"], JOB);
        assert_eq!(chunk["part"], 0);
        assert_eq!(
            chunk["total"], 1,
            "this bounded fixture's Run fits one state chunk"
        );
        serde_json::from_slice(&STANDARD.decode(chunk["bytes"].as_str().unwrap()).unwrap()).unwrap()
    }
    async fn worker_controls(&self, child: &Child) -> runs::WorkerControls {
        let value = self
            .query(
                "runs",
                runs::RunsQuery::WorkerControls {
                    run_id: child.spec.agent.as_ref().unwrap().run_id.clone(),
                },
            )
            .await;
        serde_json::from_value(value["worker_controls"].clone()).unwrap()
    }
    async fn session_actions(&self, child: &Child) -> u32 {
        let bytes = self
            .host
            .query("runs", &runs::encode_query(&runs::RunsQuery::AgentSessions))
            .await
            .unwrap();
        let runs::RunsReply::AgentSessions(sessions) = runs::decode_reply(&bytes).unwrap() else {
            panic!("real AgentSessions");
        };
        sessions
            .iter()
            .find(|session| session.run_id == child.spec.agent.as_ref().unwrap().run_id)
            .unwrap()
            .actions
    }
    async fn revision(&self, plan: &plan::Plan) -> u64 {
        self.query(
            "pages",
            pages::PageQuery::RecordCollection {
                page_id: plan.board_page_id.clone(),
            },
        )
        .await["record_collection"]["revision"]
            .as_u64()
            .unwrap()
    }
    fn take_request(&mut self, agent_id: &str) -> saga::WorkerRequest {
        let index = self.requests.iter().position(|request| {
            let work = dispatch::decode_work_spec(&request.spec).unwrap();
            let payload: Value = serde_json::from_slice(&work.payload).unwrap();
            payload["agent_id"] == agent_id
        });
        self.requests
            .remove(index.expect("real Runs emitted requested agent execution"))
            .unwrap()
    }
}

struct VendorRequest {
    body: Value,
    reply: tokio::sync::oneshot::Sender<String>,
}
struct Vendor {
    base: String,
    requests: tokio::sync::mpsc::UnboundedReceiver<VendorRequest>,
    server: tokio::task::JoinHandle<()>,
    seen: Vec<Value>,
}
impl Drop for Vendor {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Vendor {
    async fn new() -> Self {
        let (send, requests) = tokio::sync::mpsc::unbounded_channel::<VendorRequest>();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move |Json(body): Json<Value>| {
                let send = send.clone();
                async move {
                    let (reply, receive) = tokio::sync::oneshot::channel();
                    send.send(VendorRequest { body, reply }).unwrap();
                    (
                        [("content-type", "text/event-stream")],
                        receive.await.unwrap(),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            base: format!("http://{address}"),
            requests,
            server,
            seen: vec![],
        }
    }
    async fn next(
        &mut self,
        network: &mut Network,
        child: &mut Child,
        model: &str,
    ) -> VendorRequest {
        let incoming = async {
            tokio::select! {
                request = self.requests.recv() => request.expect("Pi invoked real vendor HTTP"),
                output = &mut child.task => {
                    let output = output.unwrap();
                    std::fs::write(child.directory.join("stdout.jsonl"), &output.stdout).unwrap();
                    std::fs::write(child.directory.join("stderr.log"), &output.stderr).unwrap();
                    panic!("Pi exited before expected vendor request; inspect {}", child.directory.display());
                }
            }
        };
        let request = network.until(incoming).await;
        assert_eq!(request.body["model"], model);
        self.seen.push(request.body.clone());
        request
    }
}
impl VendorRequest {
    fn tool(self, name: &str, args: Value) {
        self.respond(Some((name, args)), "");
    }
    fn answer(self, text: &str) {
        self.respond(None, text);
    }
    fn respond(self, tool: Option<(&str, Value)>, text: &str) {
        let (block, delta, stop) = match tool {
            Some((name, args)) => (
                json!({"type":"tool_use","id":format!("call-{name}"),"name":name,"input":{}}),
                json!({"type":"input_json_delta","partial_json":args.to_string()}),
                "tool_use",
            ),
            None => (
                json!({"type":"text","text":""}),
                json!({"type":"text_delta","text":text}),
                "end_turn",
            ),
        };
        let events = [
            json!({"type":"message_start","message":{"id":"connected-message","type":"message","role":"assistant","model":self.body["model"],"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":block}),
            json!({"type":"content_block_delta","index":0,"delta":delta}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":stop,"stop_sequence":null},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ];
        let sse = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        self.reply.send(sse).unwrap();
    }
    fn last_result(&self) -> Value {
        let messages = self.body["messages"].as_array().unwrap();
        let result = messages
            .iter()
            .rev()
            .flat_map(|message| message["content"].as_array().into_iter().flatten().rev())
            .find(|block| block["type"] == "tool_result")
            .expect("real Pi returned a tool result");
        assert_ne!(result["is_error"], true, "tool failed: {result}");
        let content = &result["content"];
        let text = match content.as_str() {
            Some(text) => text.to_owned(),
            None => content
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>(),
        };
        serde_json::from_str(&text).expect("Chief tool result JSON")
    }
}

struct Child {
    task: tokio::task::JoinHandle<std::process::Output>,
    session: RunSession,
    request: saga::WorkerRequest,
    spec: compute_service::WorkspaceSpec,
    directory: PathBuf,
}
impl Drop for Child {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn start(
    network: &mut Network,
    vendor: &Vendor,
    root: &Path,
    name: &str,
    agent_id: &str,
    model: &str,
) -> Child {
    let request = network.take_request(agent_id);
    assert_eq!(request.assignee, Some(key(8)));
    network
        .submit(
            8,
            "saga",
            saga::SagaMsg::Accept {
                saga_id: request.saga_id.clone(),
                attempt: request.attempt,
            },
        )
        .await;
    let work = dispatch::decode_work_spec(&request.spec).unwrap();
    let prepared =
        compute_service::envelope::prepare(std::str::from_utf8(&work.payload).unwrap()).unwrap();
    let prompt = prepared.input.clone();
    let spec = compute_service::WorkspaceSpec {
        run_id: format!("{}:{}", request.saga_id, request.attempt),
        agent: Some(compute_service::AgentExecution {
            run_id: prepared.workspace.consensus_run_id,
            native_conversation: prepared.workspace.native_conversation,
            attempt: request.attempt,
            agent_id: agent_id.into(),
            display_name: prepared.workspace.agent_display_name,
        }),
        source: prepared.workspace.source,
        ro_mounts: prepared.workspace.skills,
    };
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let link = network.link.clone();
    let session = network
        .until(open(&link, &spec, &directory))
        .await
        .unwrap()
        .unwrap();
    let mut native = session.native_conversation.clone().unwrap();
    native.system_prompt =
        format!("You are {agent_id}. Execute only your committed conversation scope.");
    let config = directory.join("pi-config");
    std::fs::create_dir(&config).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../services/provider/src/pi");
    for file in ["conversation.ts", "ducktape.ts"] {
        std::fs::copy(source.join(file), config.join(file)).unwrap();
    }
    std::fs::write(
        config.join("conversation.json"),
        serde_json::to_vec(&native).unwrap(),
    )
    .unwrap();
    std::fs::write(config.join("auth.json"), "{}").unwrap();
    std::fs::write(config.join("models.json"), json!({"providers":{"anthropic":{
        "api":"anthropic-messages","baseUrl":vendor.base,"apiKey":"$DUCKTAPE_BROKER_TOKEN",
        "models":[{"id":model,"name":model,"reasoning":false,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":128000,"maxTokens":2048}]
    }}}).to_string()).unwrap();
    let binary = std::env::var("PI_TEST_BINARY").unwrap_or_else(|_| "pi".into());
    let mcp = std::env::var_os("DUCKTAPE_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/ducktape")
        });
    assert!(mcp.is_file(), "build the current ducktape MCP binary first");
    let bin = directory.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(mcp.canonicalize().unwrap(), bin.join("ducktape")).unwrap();
    let mut child = tokio::process::Command::new(binary)
        .args([
            "--print",
            "--mode",
            "json",
            "--no-session",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-approve",
            "--provider",
            "anthropic",
            "--model",
            model,
            "-e",
        ])
        .arg(config.join("conversation.ts"))
        .current_dir(&directory)
        .env_clear()
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .env("HOME", &directory)
        .env("TMPDIR", root)
        .env("PI_CODING_AGENT_DIR", &config)
        .env("DUCKTAPE_NODE", network.link.base())
        .env("DUCKTAPE_RUN_AGENT", agent_id)
        .env("DUCKTAPE_RUN_ID", &spec.agent.as_ref().unwrap().run_id)
        .env("DUCKTAPE_RUN_ACTION_URL", &session.action_url)
        .env("DUCKTAPE_RUN_ACTION_TOKEN", &session.action_token)
        .env("DUCKTAPE_BROKER_TOKEN", "connected-offline-not-a-secret")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("/ducktape-conversation {}\n", STANDARD.encode(prompt)).as_bytes())
        .await
        .unwrap();
    let task = tokio::spawn(async move { child.wait_with_output().await.unwrap() });
    Child {
        task,
        session,
        request,
        spec,
        directory,
    }
}

async fn finish(network: &mut Network, child: &mut Child) -> Vec<Value> {
    let output = network.until(&mut child.task).await.unwrap();
    std::fs::write(child.directory.join("stdout.jsonl"), &output.stdout).unwrap();
    std::fs::write(child.directory.join("stderr.log"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "Pi failed; inspect {}",
        child.directory.display()
    );
    let frames: Vec<Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        !frames
            .iter()
            .any(|frame| frame["type"] == "native_conversation_error")
    );
    let handled = frames
        .iter()
        .any(|frame| frame["type"] == "native_conversation_handled");
    let text = frames
        .iter()
        .rev()
        .find(|frame| frame["type"] == "message_end" && frame["message"]["role"] == "assistant")
        .map(|frame| {
            frame["message"]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>()
        })
        .unwrap_or_default();
    assert!(
        handled || !text.is_empty(),
        "native stdout must carry the completion actually delivered"
    );
    // Oracle transport only: the answer and disposition come from real Pi
    // protocol frames, and the workspace is unchanged by this read-only task.
    let result = json!({"ducktape_runner_result":1,"response_text":text,"native_input_handled":handled,
        "workspace_receipt":compute_service::WorkspaceReceipt::no_changes(&child.spec)});
    network
        .submit(
            8,
            "saga",
            saga::SagaMsg::OracleResult {
                saga_id: child.request.saga_id.clone(),
                attempt: child.request.attempt,
                usage: None,
                outcome: Ok(sdk::wire::encode(&result)),
            },
        )
        .await;
    frames
}

async fn bootstrap(network: &mut Network, repo: &Path) -> plan::Plan {
    for (seed, name) in [(1, "Alice"), (2, "Bob")] {
        network
            .submit(
                seed,
                "identity",
                identity::IdentityMsg::Create {
                    name: name.into(),
                    scheme: identity::KeyScheme::Ed25519,
                },
            )
            .await;
    }
    network
        .submit(
            8,
            "capability",
            capability::CapabilityMsg::Announce {
                capabilities: vec!["pi".into(), "worker-pi".into()],
                resources: Default::default(),
            },
        )
        .await;
    network
        .submit(
            1,
            "agent",
            agent::AgentMsg::Provision {
                request_id: "worker".into(),
                name: "Independent worker".into(),
                program: runs::model_program("worker"),
            },
        )
        .await;
    network
        .submit(
            1,
            "runs",
            runs::RunsMsg::ConfigureModel {
                operation: runs::ModelMsg::RegisterModel {
                    account: 3,
                    agent_id: "worker".into(),
                    display_name: "Independent worker".into(),
                    capability: "worker-pi".into(),
                    recipe_hash: None,
                    skills: None,
                },
            },
        )
        .await;
    network
        .submit(1, "runs", runs::RunsMsg::EnableJobWorker { enabled: true })
        .await;
    let plan = plan::Plan::new(1, "connected", None, "worker").unwrap();
    let prefix = format!("{}/package", plan.root());
    let mut changes = vec![];
    // Actual shipping package, not the lifecycle harness's placeholder index.ts.
    for entry in std::fs::read_dir(repo.join("agents/chief")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        let shipping = path.is_file()
            && (name == "package.json"
                || (name.ends_with(".ts")
                    && !name.contains(".test.")
                    && !name.contains("integration")
                    && name != "test-support.ts"));
        if !shipping {
            continue;
        }
        changes.push(files::Change::Put {
            path: format!("{prefix}/{name}"),
            exec: false,
            meta: Default::default(),
            content: files::Content::Inline {
                b64: STANDARD.encode(std::fs::read(&path).unwrap()),
            },
        });
    }
    changes.push(files::Change::Put {
        path: format!("{prefix}/chief.config.json"),
        exec: false,
        meta: Default::default(),
        content: files::Content::Inline {
            b64: STANDARD.encode(plan.config().to_string()),
        },
    });
    network
        .submit(
            1,
            "files",
            files::FilesMsg::Commit {
                base_snapshot: None,
                message: "Actual Chief package".into(),
                changes,
            },
        )
        .await;
    let refs = network.query("files", files::FilesQuery::Refs {}).await;
    let package = run_envelope::ConversationPackage {
        name: "chief".into(),
        source_prefix: prefix,
        source_snapshot: refs["refs"]["head"].as_str().unwrap().into(),
    };
    let (program, _) = plan::program(&plan, &package);
    network
        .submit(
            1,
            "agent",
            agent::AgentMsg::Provision {
                request_id: plan.namespace.clone(),
                name: "Chief".into(),
                program,
            },
        )
        .await;
    network
        .submit(
            1,
            "agent",
            agent::AgentMsg::Initialize {
                account: 4,
                request_id: "connected-initialize".into(),
            },
        )
        .await;
    assert!(
        network.requests.is_empty(),
        "initializer does not call an LLM"
    );
    plan
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires installed Pi and current ducktape MCP binary; run explicitly in Chief gates"]
async fn actual_chief_and_independent_native_worker_share_one_real_host() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let artifacts = repo.join("target/chief-gates/connected");
    std::fs::create_dir_all(&artifacts).unwrap();
    let root = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&artifacts)
        .unwrap()
        .keep();
    std::fs::write(
        artifacts.join("latest-run.txt"),
        root.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let mut network = Network::new(&root).await;
    let mut vendor = Vendor::new().await;
    let report_text = format!("{REPORT}{}", "evidence🦆".repeat(230));
    assert!(report_text.len() <= 4096 && report_text.chars().count() > 2000);
    let plan = bootstrap(&mut network, &repo).await;
    for (seed, id, text) in [
        (
            1,
            "alice-input",
            "Inspect /shared/connected only; report evidence, do not accept it yourself.",
        ),
        (
            2,
            "bob-input",
            "I also need this read-only review; Chief must explicitly accept after reading.",
        ),
    ] {
        network.height += 1;
        let frame = node::encode_frame(
            &signer(seed),
            network.height,
            &message(
                "chat",
                chat::ChatMsg::PostMessage {
                    channel_id: plan.channel_id.clone(),
                    message_id: id.into(),
                    blocks: vec![chat::Block::paragraph(text)],
                    thread: None,
                },
            ),
        );
        let (origin, msg) = node::decode_frame(&frame).unwrap();
        let outcome = network
            .host
            .submit_at(at(network.height, origin), msg)
            .await
            .unwrap();
        network.events(outcome.events);
    }
    network.drain().await;
    let mut chief = start(
        &mut network,
        &vendor,
        &root,
        "chief-intake",
        &plan.namespace,
        "chief-model",
    )
    .await;
    let first = vendor.next(&mut network, &mut chief, "chief-model").await;
    assert!(
        first.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"].as_str().unwrap().starts_with("chief_"))
    );
    let intake = first.body.to_string();
    assert!(
        intake.contains("alice-input"),
        "first person's frozen input reaches actual Chief"
    );
    assert!(
        !intake.contains("bob-input"),
        "second person queues behind the active immutable turn"
    );
    first.tool("chief_task", json!({"operationId":"connected-create","expectedRevision":network.revision(&plan).await,
        "task":{"id":TASK,"key":"connected-evidence","title":"Review native worker evidence","brief":"Inspect /shared/connected and report evidence; no writes or scope expansion.","scope":["shared/connected"],"access":"read","dependencies":[],"origin":"user"}}));
    let dispatch = vendor.next(&mut network, &mut chief, "chief-model").await;
    dispatch.last_result();
    dispatch.tool("chief_dispatch", json!({"operationId":JOB,"expectedRevision":network.revision(&plan).await,"taskId":TASK,"fresh":true}));
    let chief_final = vendor.next(&mut network, &mut chief, "chief-model").await;
    chief_final.last_result();
    let mut worker = start(
        &mut network,
        &vendor,
        &root,
        "worker",
        "worker",
        "worker-model",
    )
    .await;
    let worker_first = vendor.next(&mut network, &mut worker, "worker-model").await;
    let worker_prompt = worker_first.body.to_string();
    assert!(worker_prompt.contains("/shared/connected") && worker_prompt.contains("read"));
    assert!(
        !worker_prompt.contains("alice-input"),
        "worker scope is its Job brief, not Chief's human inbox"
    );
    assert_ne!(
        chief.spec.agent.as_ref().unwrap().run_id,
        worker.spec.agent.as_ref().unwrap().run_id
    );
    assert_ne!(
        chief
            .session
            .native_conversation
            .as_ref()
            .unwrap()
            .conversation_id,
        worker
            .session
            .native_conversation
            .as_ref()
            .unwrap()
            .conversation_id
    );
    chief_final.answer("Independent Job dispatched; completion requires review.");
    finish(&mut network, &mut chief).await;
    drop(chief);
    assert!(
        !worker.task.is_finished(),
        "worker remains alive after Chief's process and signer are dropped"
    );
    network
        .submit(
            2,
            "chat",
            chat::ChatMsg::EditMessage {
                channel_id: plan.channel_id.clone(),
                seq: 2,
                blocks: vec![chat::Block::paragraph("MUTATED AFTER FROZEN ADMISSION")],
                base_rev: None,
            },
        )
        .await;
    let mut second_member = start(
        &mut network,
        &vendor,
        &root,
        "chief-second-member",
        &plan.namespace,
        "chief-model",
    )
    .await;
    let second_reply = vendor
        .next(&mut network, &mut second_member, "chief-model")
        .await;
    let second_prompt = second_reply.body.to_string();
    assert!(
        second_prompt.contains("bob-input")
            && second_prompt.contains("I also need this read-only review")
    );
    assert!(
        !second_prompt.contains("MUTATED AFTER FROZEN ADMISSION"),
        "later live Chat edits cannot rewrite queued provenance"
    );
    second_reply
        .answer("Both members' requirements are recorded; the independent Job remains running.");
    finish(&mut network, &mut second_member).await;
    assert!(!worker.task.is_finished());
    worker_first.tool(
        "ducktape_report_job",
        json!({"operation_id":"worker-checkpoint","kind":"checkpoint","payload":CHECKPOINT}),
    );
    let replay = vendor.next(&mut network, &mut worker, "worker-model").await;
    let checkpoint = replay.last_result();
    assert_eq!(network.session_actions(&worker).await, 1);
    replay.tool(
        "ducktape_report_job",
        json!({"operation_id":"worker-checkpoint","kind":"checkpoint","payload":CHECKPOINT}),
    );
    let worker_report = vendor.next(&mut network, &mut worker, "worker-model").await;
    assert_eq!(
        worker_report.last_result(),
        checkpoint,
        "authenticated retry returns the same committed Tasks report"
    );
    assert_eq!(
        network.session_actions(&worker).await,
        1,
        "exact report replay is free against the32-write session budget"
    );
    assert_eq!(
        network.worker_controls(&worker).await.reports.len(),
        1,
        "replay emits no duplicate Tasks report"
    );
    assert_eq!(checkpoint["report"]["payload"], CHECKPOINT);
    assert_eq!(
        checkpoint["report"]["attempt"], 1,
        "Tasks claim attempt differs from Saga execution attempt"
    );
    assert_eq!(worker.request.attempt, 0);
    // Hold the worker at its next vendor request while Chief consumes the
    // committed checkpoint. No model request is allowed in this Chief turn.
    let calls = vendor.seen.len();
    let mut progress = start(
        &mut network,
        &vendor,
        &root,
        "chief-progress",
        &plan.namespace,
        "chief-model",
    )
    .await;
    let frozen = progress.session.native_conversation.as_ref().unwrap().events.iter().find(|event|
        event.input["event"]["kind"] == "attribution"
            && event.input["event"]["content"]["attribution"]["source"]["kind"] == "job_event"
            && event.input["event"]["content"]["source"]["operation"]["checkpoint"]["operation_id"] == "worker-checkpoint"
    ).expect("Tasks attribution becomes a frozen native job_event");
    assert_eq!(frozen.actor, json!({"Module":"tasks"}));
    let source = &frozen.input["event"]["content"]["source"];
    assert_eq!(source["actor"], json!({"module":"runs"}));
    assert_eq!(source["job_attempt"], 1);
    assert_eq!(source["operation"]["checkpoint"]["payload"], CHECKPOINT);
    std::fs::write(
        root.join("frozen-checkpoint.json"),
        serde_json::to_vec_pretty(frozen).unwrap(),
    )
    .unwrap();
    let progress_frames = tokio::select! {
        frames = finish(&mut network, &mut progress) => frames,
        request = vendor.requests.recv() => panic!("routine checkpoint unexpectedly called the LLM: {:?}", request.map(|request| request.body)),
    };
    assert!(
        progress_frames
            .iter()
            .any(|frame| frame["type"] == "native_conversation_handled")
    );
    assert_eq!(
        vendor.seen.len(),
        calls,
        "routine progress must not invoke the LLM"
    );
    assert_eq!(network.task(&plan).await["status"], "running");
    let progress_state = network.board_run(&plan).await;
    assert_eq!(
        progress_state["progress"]["summary"],
        "Independent worker still running"
    );
    assert_eq!(progress_state["progress"]["next"], "Finish evidence review");
    assert!(
        progress_state["progress"]["source"]["hash"].is_string(),
        "routine raw checkpoint is retained in Files"
    );
    std::fs::write(
        root.join("canonical-progress.json"),
        serde_json::to_vec_pretty(&progress_state).unwrap(),
    )
    .unwrap();
    assert!(
        network.task(&plan).await.get("acceptance").is_none(),
        "raw report never auto-accepts"
    );
    drop(progress);
    worker_report.tool(
        "ducktape_report_job",
        json!({"operation_id":"worker-report","kind":"report","payload":report_text}),
    );
    let worker_final = vendor.next(&mut network, &mut worker, "worker-model").await;
    let report = worker_final.last_result();
    assert_eq!(report["report"]["payload"], report_text);
    assert_eq!(network.session_actions(&worker).await, 2);
    assert_eq!(network.worker_controls(&worker).await.reports.len(), 2);
    let mut claim = start(
        &mut network,
        &vendor,
        &root,
        "chief-raw-report",
        &plan.namespace,
        "chief-model",
    )
    .await;
    let claim_reply = vendor.next(&mut network, &mut claim, "chief-model").await;
    assert_eq!(network.task(&plan).await["status"], "running");
    assert!(
        network.task(&plan).await.get("acceptance").is_none(),
        "raw worker claim is neither settlement nor acceptance"
    );
    claim_reply.answer("Worker's report is a claim; await terminal settlement before acceptance.");
    finish(&mut network, &mut claim).await;
    worker_final.answer(&report_text);
    finish(&mut network, &mut worker).await;
    let mut review = start(
        &mut network,
        &vendor,
        &root,
        "chief-review",
        &plan.namespace,
        "chief-model",
    )
    .await;
    let review_first = vendor.next(&mut network, &mut review, "chief-model").await;
    assert_eq!(network.task(&plan).await["status"], "review");
    assert!(network.task(&plan).await.get("acceptance").is_none());
    assert!(
        !review_first.body["system"].to_string().contains(REPORT),
        "raw evidence is on-demand, not injected in the system prompt"
    );
    review_first.tool("chief_report", json!({"runId":JOB,"offset":0,"limit":2000}));
    let continuation = vendor.next(&mut network, &mut review, "chief-model").await;
    let readback = continuation.last_result();
    assert!(
        readback.to_string().contains(REPORT),
        "real reader retrieves real retained Files bytes"
    );
    let artifact = readback["data"]["artifact"].clone();
    assert!(
        artifact.is_object(),
        "reader returns immutable artifact identity: {readback}"
    );
    assert_eq!(
        readback["data"]["text"].as_str().unwrap().chars().count(),
        2000
    );
    assert_eq!(readback["data"]["partialRead"], true);
    assert_eq!(
        readback["data"]["provenance"],
        "untrusted_worker_claim_not_accepted"
    );
    continuation.tool("chief_report", json!({"runId":JOB,"artifact":artifact,"anchor":readback["data"]["anchor"],"offset":readback["data"]["nextOffset"],"limit":2000}));
    let accept = vendor.next(&mut network, &mut review, "chief-model").await;
    let last_window = accept.last_result();
    assert_eq!(last_window["data"]["artifact"], artifact);
    assert_eq!(last_window["data"]["nextOffset"], Value::Null);
    // The reader hands back the retained bytes themselves — the terminal claim
    // Runs committed, delivery wrapper and all — and its two windows join into
    // exactly the artifact whose id is that content's digest. Chief never
    // reshapes an untrusted claim into a friendlier one.
    let full = format!(
        "{}{}",
        readback["data"]["text"].as_str().unwrap(),
        last_window["data"]["text"].as_str().unwrap()
    );
    assert_eq!(
        artifact["fileId"],
        json!(format!(
            "report-{}",
            duckfs_core::to_hex(&Sha256::digest(full.as_bytes()))
        ))
    );
    assert!(
        full.contains(&report_text),
        "the worker's own words survive verbatim inside the committed claim"
    );
    assert_eq!(
        network.task(&plan).await["status"],
        "review",
        "reading claims does not accept them"
    );
    accept.tool("chief_accept", json!({"operationId":"connected-accept","expectedRevision":network.revision(&plan).await,"taskId":TASK,"outcome":"Reviewed the authenticated worker evidence.","evidence":[artifact]}));
    let final_reply = vendor.next(&mut network, &mut review, "chief-model").await;
    final_reply.last_result();
    assert_eq!(network.task(&plan).await["status"], "done");
    assert_eq!(network.task(&plan).await["acceptance"]["runId"], JOB);
    final_reply.answer("Evidence reviewed and explicitly accepted.");
    finish(&mut network, &mut review).await;
    for (id, expected_tool) in [
        (&plan.namespace, "chief_accept"),
        (
            &worker
                .session
                .native_conversation
                .as_ref()
                .unwrap()
                .conversation_id,
            "ducktape_report_job",
        ),
    ] {
        let view = network.conversation(id).await;
        let history = view
            .history
            .expect("actual native history is durably committed in Runs");
        let file = network
            .query(
                "files",
                files::FilesQuery::Read {
                    path: format!("{}/{}", view.history_prefix, view.session_path),
                    snapshot: Some(history.snapshot),
                    offset: 0,
                    len: 1024 * 1024,
                },
            )
            .await;
        assert_eq!(file["read"]["eof"], true);
        let bytes = STANDARD
            .decode(file["read"]["b64"].as_str().unwrap())
            .unwrap();
        assert!(
            std::str::from_utf8(&bytes).unwrap().contains(expected_tool),
            "native session bytes are read back from real Files"
        );
        std::fs::write(root.join(format!("{expected_tool}-committed.jsonl")), bytes).unwrap();
    }
    std::fs::write(
        root.join("vendor-requests.json"),
        serde_json::to_vec_pretty(&vendor.seen).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("final-task.json"),
        serde_json::to_vec_pretty(&network.task(&plan).await).unwrap(),
    )
    .unwrap();
    std::fs::write(
        artifacts.join("latest-run.txt"),
        root.to_string_lossy().as_bytes(),
    )
    .unwrap();
}
