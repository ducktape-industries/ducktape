//! Bidirectional control of one already-assigned run. The registry exists only
//! in the executing process; an expected provider turn fences every user input.
use crate::{OutputLine, OutputSink, OutputStream, RunContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Input {
    Steer {
        expected_turn: String,
        text: String,
    },
    Interrupt {
        expected_turn: String,
    },
    Approve {
        expected_turn: String,
        request_id: String,
        allow: bool,
    },
}
struct Offered {
    input: Input,
    reply: oneshot::Sender<Result<Value, String>>,
}
struct Entry {
    sender: mpsc::Sender<Offered>,
    ready: Option<Value>,
    approvals: HashMap<String, Value>,
}
type Registry = HashMap<String, Entry>;

/// Reannounce live controls after the compute socket reconnects.
pub fn active() -> Vec<(String, Value)> {
    sessions()
        .lock()
        .expect("run sessions")
        .iter()
        .flat_map(|(key, entry)| {
            entry
                .ready
                .iter()
                .chain(entry.approvals.values())
                .cloned()
                .map(|value| (key.clone(), value))
                .collect::<Vec<_>>()
        })
        .collect()
}
static SESSIONS: OnceLock<Mutex<Registry>> = OnceLock::new();
fn sessions() -> &'static Mutex<Registry> {
    SESSIONS.get_or_init(Default::default)
}

pub async fn control(run: &str, input: Input) -> Result<Value, String> {
    let sender = sessions()
        .lock()
        .expect("run sessions")
        .get(run)
        .map(|entry| entry.sender.clone())
        .ok_or("This run has no active control connection on this executor.")?;
    let (reply, result) = oneshot::channel();
    sender
        .try_send(Offered { input, reply })
        .map_err(|_| "Run control is busy or closed.")?;
    result
        .await
        .map_err(|_| "The run ended before acknowledging the input.".to_string())?
}
struct Registration {
    key: String,
    sender: mpsc::Sender<Offered>,
    ctx: RunContext,
    sink: Option<OutputSink>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut registry = sessions().lock().expect("run sessions");
        let own = registry
            .get(&self.key)
            .is_some_and(|sender| sender.sender.same_channel(&self.sender));
        if own {
            registry.remove(&self.key);
        }
        drop(registry);
        if own {
            emit(
                &self.sink,
                &self.ctx,
                json!({"type":"run_control","state":"closed"}),
            );
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Codex,
    Claude,
}

/// A same-turn App Server instruction, fenced by the observed turn id.
fn codex_steer(id: u64, thread: &str, turn: &str, text: &str) -> Value {
    json!({"id":id,"method":"turn/steer","params":{"threadId":thread,"expectedTurnId":turn,"input":[{"type":"text","text":text}]}})
}

fn emit(sink: &Option<OutputSink>, ctx: &RunContext, value: Value) {
    if value["type"] == "run_control"
        && let Some(key) = &ctx.run_key
    {
        let mut registry = sessions().lock().expect("run sessions");
        if let Some(entry) = registry.get_mut(key) {
            let id = value["request_id"].as_str().unwrap_or_default();
            match value["state"].as_str() {
                Some("approval") => {
                    if entry.approvals.len() < 16 {
                        entry.approvals.insert(id.into(), value.clone());
                    }
                }
                Some("approval_resolved") => {
                    entry.approvals.remove(id);
                }
                _ => {}
            }
        }
    }
    if let Some(sink) = sink {
        sink(
            ctx,
            OutputLine {
                stream: OutputStream::Stdout,
                line: value.to_string(),
            },
        );
    }
}
// A separate, owned writer drains FIFO input while the driver keeps reading
// stdout/stderr. Dropping the driver aborts it and closes the provider's stdin.
type Reply = oneshot::Sender<Result<Value, String>>;
struct Writer {
    sender: mpsc::Sender<(Value, Option<Reply>)>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}
impl Writer {
    fn new(mut stdin: Box<dyn AsyncWrite + Send + Unpin>) -> Self {
        let (sender, mut receiver) = mpsc::channel::<(Value, Option<Reply>)>(32);
        let task = tokio::spawn(async move {
            while let Some((frame, reply)) = receiver.recv().await {
                stdin
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .map_err(|_| "provider input closed".to_string())?;
                stdin
                    .flush()
                    .await
                    .map_err(|_| "provider input closed".to_string())?;
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(json!({"status":"responded"})));
                }
            }
            Ok(())
        });
        Self { sender, task }
    }
    fn respond(&self, frame: Value, reply: Reply) -> Result<(), String> {
        self.sender
            .try_send((frame, Some(reply)))
            .map_err(|_| "Provider input is busy or closed.".into())
    }
    fn send(&self, frame: Value) -> Result<(), String> {
        self.sender
            .try_send((frame, None))
            .map_err(|_| "Provider input is busy or closed.".into())
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Both boundaries must arrive before injecting the replacement instruction.
// Claude may emit its interrupted result before acknowledging the interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    Both,
    Ack,
    Result,
    Ready,
}
enum BoundaryEvent {
    InterruptAcknowledged,
    ResultReceived,
}
impl Boundary {
    fn step(self, event: BoundaryEvent) -> Self {
        match event {
            BoundaryEvent::InterruptAcknowledged => self.ack(),
            BoundaryEvent::ResultReceived => self.result(),
        }
    }
    fn ack(self) -> Self {
        match self {
            Self::Both => Self::Result,
            Self::Ack => Self::Ready,
            Self::Result => Self::Result,
            Self::Ready => Self::Ready,
        }
    }
    fn result(self) -> Self {
        match self {
            Self::Both => Self::Ack,
            Self::Result => Self::Ready,
            Self::Ack => Self::Ack,
            Self::Ready => Self::Ready,
        }
    }
}
struct ClaudeSteer {
    id: String,
    text: String,
    boundary: Boundary,
}
fn message_id(id: u64) -> String {
    format!("00000000-0000-4000-8000-{id:012x}")
}
fn claude_user(id: &str, text: &str) -> Value {
    json!({"type":"user","uuid":id,"message":{"role":"user","content":text},"parent_tool_use_id":null})
}
async fn line(
    reader: &mut BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    bytes: &mut Vec<u8>,
) -> Result<Option<String>, String> {
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|_| "provider output failed")?;
        if available.is_empty() {
            return Ok((!bytes.is_empty())
                .then(|| String::from_utf8_lossy(&std::mem::take(bytes)).into_owned()));
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|n| n + 1);
        let n = end.unwrap_or(available.len());
        if bytes.len() + n > 256 * 1024 {
            return Err("provider event exceeds 256 KiB".into());
        }
        bytes.extend_from_slice(&available[..n]);
        reader.consume(n);
        if end.is_some() {
            return Ok(Some(
                String::from_utf8_lossy(&std::mem::take(bytes))
                    .trim_end()
                    .into(),
            ));
        }
    }
}

pub(crate) type Pipes = (
    Box<dyn AsyncWrite + Send + Unpin>,
    Box<dyn AsyncRead + Send + Unpin>,
    Box<dyn AsyncRead + Send + Unpin>,
);

pub(crate) async fn drive(
    protocol: Protocol,
    prompt: &str,
    (stdin, stdout, stderr): Pipes,
    ctx: &RunContext,
    sink: Option<OutputSink>,
    (idle, hard): (Duration, tokio::time::Instant),
    broker: Option<&broker_host::BrokerInvocation>,
) -> Result<crate::Invocation, String> {
    let (sender, mut offers) = mpsc::channel(16);
    let _registration = ctx.run_key.clone().map(|key| {
        sessions().lock().expect("run sessions").insert(
            key.clone(),
            Entry {
                sender: sender.clone(),
                ready: None,
                approvals: HashMap::new(),
            },
        );
        Registration {
            key,
            sender,
            ctx: ctx.clone(),
            sink: sink.clone(),
        }
    });
    let initial = match protocol {
        Protocol::Codex => {
            json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"ducktape-run","version":env!("CARGO_PKG_VERSION")}}})
        }
        Protocol::Claude => {
            json!({"type":"control_request","request_id":"1","request":{"subtype":"initialize","hooks":null}})
        }
    };
    let mut writer = Writer::new(stdin);
    writer.send(initial)?;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = BufReader::new(stderr);
    let mut thread = String::new();
    let mut turn = String::new();
    let mut answer = String::new();
    let mut usage = None;
    let mut steering: Option<ClaudeSteer> = None;
    let mut stop_id: Option<String> = None;
    let mut stdout_bytes = 0usize;
    let mut explicit_deadline = broker.map(|invocation| invocation.idle_deadline.clone());
    let mut next_id = 10u64;
    let mut pending: HashMap<String, oneshot::Sender<Result<Value, String>>> = HashMap::new();
    let mut approvals: HashMap<String, Value> = HashMap::new();
    let mut last_activity = tokio::time::Instant::now();
    let mut err_open = true;
    let mut out_pending = Vec::new();
    let mut err_pending = Vec::new();
    let mut last_stderr = String::new();
    loop {
        let explicit = explicit_deadline
            .as_ref()
            .and_then(|deadline| *deadline.borrow());
        let deadline = crate::effective_provider_deadline(last_activity, idle, explicit, hard);
        if let Some(steer) = &steering
            && steer.boundary == Boundary::Ready
        {
            writer.send(claude_user(&steer.id, &steer.text))?;
            steering = None;
        }
        tokio::select! {
            _ = crate::cancellation_requested(ctx.cancellation.as_ref()) => return Err("run cancelled".into()),
            _ = tokio::time::sleep_until(deadline) => {
                let now = tokio::time::Instant::now();
                let granted = broker.is_some_and(|invocation| invocation.continue_after_timeout_wake(last_activity,idle,hard,now));
                if granted { continue; }
                return Err("provider session timed out".into());
            },
            changed = async { match explicit_deadline.as_mut() { Some(deadline) => deadline.changed().await, None => std::future::pending().await } } => {
                if changed.is_err() { explicit_deadline = None; }
            },
            _ = &mut writer.task => return Err("provider input closed".into()),
            read = line(&mut stderr, &mut err_pending), if err_open => {
                match read? {
                    Some(line) => { last_stderr = line.clone(); last_activity = tokio::time::Instant::now(); if let Some(sink) = &sink { sink(ctx, OutputLine { stream:OutputStream::Stderr,line }); } }
                    None => err_open = false,
                }
            }
            Some(offer) = offers.recv() => {
                if offer.reply.is_closed() { continue; }
                if !pending.is_empty() || steering.is_some() { let _ = offer.reply.send(Err("Too many unacknowledged inputs.".into())); continue; }
                let expected = match &offer.input {
                    Input::Steer { expected_turn, .. } | Input::Interrupt { expected_turn } | Input::Approve { expected_turn, .. } => expected_turn,
                };
                let current = !turn.is_empty() && expected == &turn;
                if !current { let _ = offer.reply.send(Err("The active turn changed.".into())); continue; }
                let id = next_id; next_id += 1;
                emit(&sink,ctx,json!({"type":"run_control","state":"input","input":offer.input}));
                let frame = match offer.input {
                    Input::Steer { text, .. } => {
                        let valid = !text.trim().is_empty() && text.len() <= 8192;
                        if !valid { let _ = offer.reply.send(Err("Message must be 1..8192 bytes.".into())); continue; }
                        match protocol {
                            Protocol::Codex => codex_steer(id,&thread,&turn,&text),
                            Protocol::Claude => {
                                steering = Some(ClaudeSteer { id:message_id(id),text:text.clone(),boundary:Boundary::Both });
                                json!({"type":"control_request","request_id":id.to_string(),"request":{"subtype":"interrupt"}})
                            },
                        }
                    }
                    Input::Interrupt { .. } => { stop_id = Some(id.to_string()); match protocol {
                        Protocol::Codex => json!({"id":id,"method":"turn/interrupt","params":{"threadId":thread,"turnId":turn}}),
                        Protocol::Claude => json!({"type":"control_request","request_id":id.to_string(),"request":{"subtype":"interrupt"}}),
                    }
                    }
                    Input::Approve { request_id, allow, .. } => {
                        let Some(request) = approvals.remove(&request_id) else { let _ = offer.reply.send(Err("Approval is no longer pending.".into())); continue; };
                        let frame = match protocol {
                            Protocol::Codex => json!({"id":request["id"],"result":{"decision":if allow {"accept"} else {"decline"}}}),
                            Protocol::Claude => json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":if allow {json!({"behavior":"allow","updatedInput":request["request"]["input"]})} else {json!({"behavior":"deny","message":"Declined by the user"})}}}),
                        };
                        writer.respond(frame,offer.reply)?;
                        emit(&sink,ctx,json!({"type":"run_control","state":"approval_resolved","turn":turn,"request_id":request_id}));
                        continue;
                    }
                };
                let pending_id = match &steering { Some(steer) => steer.id.clone(), None => id.to_string() };
                pending.insert(pending_id,offer.reply);
                writer.send(frame)?;
            }
            read = line(&mut stdout, &mut out_pending) => {
                let raw = read?.ok_or_else(|| format!("provider exited before completing the run: {last_stderr}"))?;
                stdout_bytes = stdout_bytes.saturating_add(raw.len());
                if stdout_bytes > crate::MAX_RUN_OUTPUT_BYTES { return Err("provider output exceeded the run limit".into()); }
                let Ok(frame) = serde_json::from_str::<Value>(&raw) else { continue; };
                last_activity = tokio::time::Instant::now();
                emit(&sink,ctx,frame.clone());
                match protocol {
                    Protocol::Codex => {
                        let method = frame["method"].as_str().unwrap_or_default();
                        let is_reply = method.is_empty();
                        if is_reply {
                            let id = frame["id"].as_u64().unwrap_or(0);
                            if let Some(reply) = pending.remove(&id.to_string()) {
                                let result = match frame.get("error") { Some(error) => Err(error["message"].as_str().unwrap_or("Input refused").into()), None => Ok(frame["result"].clone()) };
                                let _ = reply.send(result); continue;
                            }
                            if frame.get("error").is_some() { return Err(frame["error"]["message"].as_str().unwrap_or("Session initialization failed").into()); }
                            match id {
                                1 => {
                                    writer.send(json!({"method":"initialized"}))?;
                                    writer.send(json!({"id":2,"method":"thread/start","params":{"approvalPolicy":"on-request"}}))?;
                                }
                                2 => {
                                    thread = frame["result"]["thread"]["id"].as_str().ok_or("missing provider thread")?.into();
                                    writer.send(json!({"id":3,"method":"turn/start","params":{"threadId":thread,"input":[{"type":"text","text":prompt}]}}))?;
                                }
                                3 => { turn = frame["result"]["turn"]["id"].as_str().ok_or("missing provider turn")?.into(); ready(&sink,ctx,&turn,true); }
                                _ => {}
                            }
                        } else {
                            match method {
                                "thread/tokenUsage/updated" => {
                                    let total = &frame["params"]["tokenUsage"]["total"];
                                    usage = Some(crate::TokenUsage {
                                        input_tokens:total["inputTokens"].as_u64().unwrap_or(0),
                                        cached_input_tokens:total["cachedInputTokens"].as_u64().unwrap_or(0),
                                        output_tokens:total["outputTokens"].as_u64().unwrap_or(0),
                                        reasoning_output_tokens:total["reasoningOutputTokens"].as_u64().unwrap_or(0),
                                        ..Default::default()
                                    });
                                }
                                "turn/started" => { turn = frame["params"]["turn"]["id"].as_str().unwrap_or_default().into(); ready(&sink,ctx,&turn,true); }
                                "item/completed" if frame["params"]["item"]["type"] == "agentMessage" => { answer = frame["params"]["item"]["text"].as_str().unwrap_or_default().into(); }
                                "turn/completed" => {
                                    let status = frame["params"]["turn"]["status"].as_str().unwrap_or_default();
                                    if status == "interrupted" { acknowledge_stop(&mut stop_id,&mut pending); }
                                    if status != "completed" { return Err(format!("provider turn {status}")); }

                                    return Ok(crate::Invocation { text:answer, usage });
                                }
                                "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                                    if approvals.len() >= 16 { return Err("Too many pending approvals".into()); }
                                    let id = frame["id"].to_string(); approvals.insert(id.clone(),frame.clone()); approval(&sink,ctx,&turn,&id,&frame["params"]);
                                }
                                _ => {}
                            }
                        }
                    }
                    Protocol::Claude => {
                        match frame["type"].as_str().unwrap_or_default() {
                            "control_response" => {
                                let id = frame["response"]["request_id"].as_str().unwrap_or_default();
                                if id == "1" {
                                    writer.send(json!({"type":"user","message":{"role":"user","content":prompt},"parent_tool_use_id":null}))?;
                                } else if let Some(steer) = &mut steering {
                                    if id == (next_id-1).to_string() {
                                        if frame["response"]["subtype"] == "error" { return Err("Provider refused the interrupt for steering.".into()); }
                                        steer.boundary = steer.boundary.step(BoundaryEvent::InterruptAcknowledged);
                                    }
                                } else if let Some(reply) = pending.remove(id) {
                                    let result = if frame["response"]["subtype"] == "error" { Err("Provider refused the request.".into()) } else { Ok(frame["response"].clone()) };
                                    let _ = reply.send(result);
                                }
                            }
                            "system" if frame["subtype"] == "init" => { thread = frame["session_id"].as_str().unwrap_or_default().into(); turn = format!("{}:{}",thread,next_id); ready(&sink,ctx,&turn,true); }
                            "user" => { if let Some(id) = frame["uuid"].as_str() && let Some(reply) = pending.remove(id) { let _ = reply.send(Ok(json!({"status":"accepted"}))); } }
                            "control_request" if frame["request"]["subtype"] == "can_use_tool" => { if approvals.len() >= 16 { return Err("Too many pending approvals".into()); } let id = frame["request_id"].as_str().unwrap_or_default().to_string(); approvals.insert(id.clone(),frame.clone()); approval(&sink,ctx,&turn,&id,&frame["request"]); }
                            "result" => {
                                if frame["terminal_reason"] == "aborted_streaming" { acknowledge_stop(&mut stop_id,&mut pending); }
                                if let Some(steer) = &mut steering { steer.boundary = steer.boundary.step(BoundaryEvent::ResultReceived); approvals.clear(); continue; }
                                if frame["is_error"] == true { return Err(frame["result"].as_str().unwrap_or("Provider run failed").into()); }
                                let text = frame["result"].as_str().ok_or("missing result")?.into();

                                return Ok(crate::Invocation { text, usage:crate::parse_token_usage(&raw) });
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
fn acknowledge_stop(id: &mut Option<String>, pending: &mut HashMap<String, Reply>) {
    if let Some(id) = id.take()
        && let Some(reply) = pending.remove(&id)
    {
        let _ = reply.send(Ok(json!({"status":"interrupted"})));
    }
}
fn ready(sink: &Option<OutputSink>, ctx: &RunContext, turn: &str, steers: bool) {
    let value = json!({"type":"run_control","state":"ready","turn":turn,"steers":steers});
    if let Some(key) = &ctx.run_key
        && let Some(entry) = sessions().lock().expect("run sessions").get_mut(key)
    {
        let changed = entry
            .ready
            .as_ref()
            .is_some_and(|ready| ready["turn"] != value["turn"]);
        if changed {
            entry.approvals.clear();
        }
        entry.ready = Some(value.clone());
    }
    emit(sink, ctx, value);
}
fn approval(sink: &Option<OutputSink>, ctx: &RunContext, turn: &str, id: &str, detail: &Value) {
    emit(
        sink,
        ctx,
        json!({"type":"run_control","state":"approval","turn":turn,"request_id":id,"detail":detail}),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, DuplexStream};

    async fn receive(reader: &mut BufReader<DuplexStream>) -> Value {
        let mut raw = String::new();
        assert_ne!(reader.read_line(&mut raw).await.unwrap(), 0);
        serde_json::from_str(&raw).unwrap()
    }
    async fn send(writer: &mut DuplexStream, frame: Value) {
        writer
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn ready_event(events: &mut mpsc::UnboundedReceiver<Value>) -> String {
        loop {
            let frame = events.recv().await.expect("session closed before ready");
            if frame["state"] == "ready" {
                return frame["turn"].as_str().unwrap().into();
            }
        }
    }
    fn sink() -> (Option<OutputSink>, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Some(std::sync::Arc::new(move |_, line| {
                if let Ok(value) = serde_json::from_str(&line.line) {
                    let _ = tx.send(value);
                }
            })),
            rx,
        )
    }
    #[test]
    fn claude_steer_waits_for_both_interrupt_boundaries_in_either_order() {
        assert_eq!(
            Boundary::Both
                .step(BoundaryEvent::InterruptAcknowledged)
                .step(BoundaryEvent::ResultReceived),
            Boundary::Ready
        );
        assert_eq!(
            Boundary::Both
                .step(BoundaryEvent::ResultReceived)
                .step(BoundaryEvent::InterruptAcknowledged),
            Boundary::Ready
        );
        assert_ne!(
            Boundary::Both
                .step(BoundaryEvent::InterruptAcknowledged)
                .step(BoundaryEvent::InterruptAcknowledged),
            Boundary::Ready
        );
        assert_ne!(
            Boundary::Both
                .step(BoundaryEvent::ResultReceived)
                .step(BoundaryEvent::ResultReceived),
            Boundary::Ready
        );
    }
    #[tokio::test]
    async fn codex_same_turn_steer_fences_stale_input_and_drains_large_prompt() {
        let (stdin, input) = tokio::io::duplex(128);
        let (stdout, mut output) = tokio::io::duplex(128);
        let (stderr, err) = tokio::io::duplex(128);
        let (sink, mut events) = sink();
        let ctx = RunContext {
            run_key: Some("test-codex-control".into()),
            ..Default::default()
        };
        let prompt = "한글 ".repeat(16384);
        let run = drive(
            Protocol::Codex,
            &prompt,
            (Box::new(stdin), Box::new(stdout), Box::new(stderr)),
            &ctx,
            sink,
            (
                Duration::from_secs(10),
                tokio::time::Instant::now() + Duration::from_secs(20),
            ),
            None,
        );
        let peer = async {
            let mut input = BufReader::new(input);
            assert_eq!(receive(&mut input).await["method"], "initialize");
            send(&mut output, json!({"id":1,"result":{}})).await;
            assert_eq!(receive(&mut input).await["method"], "initialized");
            assert_eq!(receive(&mut input).await["method"], "thread/start");
            send(
                &mut output,
                json!({"id":2,"result":{"thread":{"id":"thread-a"}}}),
            )
            .await;
            // Output before consuming a prompt larger than the pipe must not deadlock.
            send(
                &mut output,
                json!({"method":"notice","params":{"text":"x".repeat(8192)}}),
            )
            .await;
            assert_eq!(
                receive(&mut input).await["params"]["input"][0]["text"],
                prompt
            );
            send(
                &mut output,
                json!({"id":3,"result":{"turn":{"id":"turn-a"}}}),
            )
            .await;
            let turn = ready_event(&mut events).await;
            assert!(
                control(
                    "test-codex-control",
                    Input::Steer {
                        expected_turn: "stale".into(),
                        text: "wrong".into()
                    }
                )
                .await
                .is_err()
            );
            let offered = tokio::spawn(control(
                "test-codex-control",
                Input::Steer {
                    expected_turn: turn,
                    text: "new direction".into(),
                },
            ));
            let steer = receive(&mut input).await;
            assert_eq!(steer["method"], "turn/steer");
            assert_eq!(steer["params"]["threadId"], "thread-a");
            assert_eq!(steer["params"]["expectedTurnId"], "turn-a");
            send(
                &mut output,
                json!({"id":steer["id"],"result":{"turnId":"turn-a"}}),
            )
            .await;
            assert!(offered.await.unwrap().is_ok());
            send(&mut output,json!({"method":"thread/tokenUsage/updated","params":{"tokenUsage":{"total":{"inputTokens":20,"cachedInputTokens":3,"outputTokens":4}}}})).await;
            send(&mut output,json!({"method":"item/completed","params":{"item":{"type":"agentMessage","text":"done"}}})).await;
            send(
                &mut output,
                json!({"method":"turn/completed","params":{"turn":{"status":"completed"}}}),
            )
            .await;
            drop(err);
        };
        let (result, ()) = tokio::join!(run, peer);
        let result = result.unwrap();
        assert_eq!(result.text, "done");
        assert_eq!(result.usage.unwrap().input_tokens, 20);
        assert!(
            control(
                "test-codex-control",
                Input::Interrupt {
                    expected_turn: "turn-a".into()
                }
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn both_protocols_answer_approvals_and_confirm_stop_at_the_terminal_event() {
        for protocol in [Protocol::Codex, Protocol::Claude] {
            let key = format!("approval-{protocol:?}");
            let (stdin, input) = tokio::io::duplex(4096);
            let (stdout, mut output) = tokio::io::duplex(4096);
            let (stderr, _err) = tokio::io::duplex(4096);
            let (sink, mut events) = sink();
            let ctx = RunContext {
                run_key: Some(key.clone()),
                ..Default::default()
            };
            let run = drive(
                protocol,
                "hello",
                (Box::new(stdin), Box::new(stdout), Box::new(stderr)),
                &ctx,
                sink,
                (
                    Duration::from_secs(10),
                    tokio::time::Instant::now() + Duration::from_secs(20),
                ),
                None,
            );
            let peer = async {
                let mut input = BufReader::new(input);
                let _ = receive(&mut input).await;
                match protocol {
                    Protocol::Codex => {
                        send(&mut output, json!({"id":1,"result":{}})).await;
                        assert_eq!(receive(&mut input).await["method"], "initialized");
                        assert_eq!(receive(&mut input).await["method"], "thread/start");
                        send(&mut output, json!({"id":2,"result":{"thread":{"id":"t"}}})).await;
                        assert_eq!(receive(&mut input).await["method"], "turn/start");
                        send(&mut output, json!({"id":3,"result":{"turn":{"id":"turn"}}})).await;
                    }
                    Protocol::Claude => {
                        send(&mut output,json!({"type":"control_response","response":{"subtype":"success","request_id":"1","response":{}}})).await;
                        assert_eq!(receive(&mut input).await["type"], "user");
                        send(
                            &mut output,
                            json!({"type":"system","subtype":"init","session_id":"session"}),
                        )
                        .await;
                    }
                }
                let turn = ready_event(&mut events).await;
                for (id, allow) in [(20, true), (21, false)] {
                    let request = match protocol {
                        Protocol::Codex => {
                            json!({"id":id,"method":"item/commandExecution/requestApproval","params":{"command":"echo hi"}})
                        }
                        Protocol::Claude => {
                            json!({"type":"control_request","request_id":id.to_string(),"request":{"subtype":"can_use_tool","input":{"command":"echo hi"}}})
                        }
                    };
                    send(&mut output, request).await;
                    loop {
                        if events.recv().await.unwrap()["state"] == "approval" {
                            break;
                        }
                    }
                    let offered = tokio::spawn({
                        let key = key.clone();
                        let turn = turn.clone();
                        async move {
                            control(
                                &key,
                                Input::Approve {
                                    expected_turn: turn,
                                    request_id: id.to_string(),
                                    allow,
                                },
                            )
                            .await
                        }
                    });
                    let response = receive(&mut input).await;
                    match protocol {
                        Protocol::Codex => assert_eq!(
                            response["result"]["decision"],
                            if allow { "accept" } else { "decline" }
                        ),
                        Protocol::Claude => assert_eq!(
                            response["response"]["response"]["behavior"],
                            if allow { "allow" } else { "deny" }
                        ),
                    }
                    assert!(offered.await.unwrap().is_ok());
                }
                let stopping = tokio::spawn({
                    let key = key.clone();
                    async move {
                        control(
                            &key,
                            Input::Interrupt {
                                expected_turn: turn,
                            },
                        )
                        .await
                    }
                });
                let request = receive(&mut input).await;
                let terminal = match protocol {
                    Protocol::Codex => {
                        assert_eq!(request["method"], "turn/interrupt");
                        json!({"method":"turn/completed","params":{"turn":{"status":"interrupted"}}})
                    }
                    Protocol::Claude => {
                        assert_eq!(request["request"]["subtype"], "interrupt");
                        json!({"type":"result","is_error":true,"terminal_reason":"aborted_streaming"})
                    }
                };
                send(&mut output, terminal).await;
                assert!(stopping.await.unwrap().is_ok());
            };
            let (result, ()) = tokio::join!(run, peer);
            assert!(result.is_err());
        }
    }

    // Explicit developer smoke tests; they spend the installed CLI's model quota.
    // All children belong to this test and are reaped on success or failure.
    async fn live_cli(protocol: Protocol) {
        let (program, args) = match protocol {
            Protocol::Codex => ("codex", vec!["app-server"]),
            Protocol::Claude => (
                "claude",
                vec![
                    "-p",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--replay-user-messages",
                    "--permission-prompts",
                    "host",
                    "--strict-mcp-config",
                    "--mcp-config",
                    r#"{"mcpServers":{}}"#,
                ],
            ),
        };
        let dir = std::env::current_dir()
            .unwrap()
            .join("target/run-control-smoke");
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = Box::new(child.stdin.take().unwrap());
        let stdout = Box::new(child.stdout.take().unwrap());
        let stderr = Box::new(child.stderr.take().unwrap());
        let key = format!("live-{protocol:?}");
        let ctx = RunContext {
            run_key: Some(key.clone()),
            ..Default::default()
        };
        let (sink, mut events) = sink();
        let run = drive(
            protocol,
            "Do not use tools. Calculate the first 80 primes and their cumulative sums, carefully.",
            (stdin, stdout, stderr),
            &ctx,
            sink,
            (
                Duration::from_secs(60),
                tokio::time::Instant::now() + Duration::from_secs(90),
            ),
            None,
        );
        let input = async {
            let turn = loop {
                let Some(event) = events.recv().await else {
                    return Err("Provider closed before ready".to_string());
                };
                if event["state"] == "ready" {
                    break event["turn"].as_str().unwrap().to_owned();
                }
            };
            control(
                &key,
                Input::Steer {
                    expected_turn: turn,
                    text:
                        "Change direction immediately. Reply only HARNESS_OK. Do not use any tools."
                            .into(),
                },
            )
            .await
        };
        let (result, accepted) = tokio::join!(run, input);
        let exited = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        if exited.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let result = result.unwrap_or_else(|error| panic!("run: {error}; input: {accepted:?}"));
        assert!(accepted.is_ok(), "input: {accepted:?}");
        assert!(
            result.text.contains("HARNESS_OK"),
            "answer: {}",
            result.text
        );
        assert!(result.usage.is_some());
        assert!(
            exited
                .expect("provider exits on stdin EOF")
                .unwrap()
                .success()
        );
    }
    #[tokio::test]
    #[ignore = "uses an installed authenticated Codex CLI and model quota"]
    async fn live_codex_same_run_steering() {
        live_cli(Protocol::Codex).await;
    }
    #[tokio::test]
    #[ignore = "uses an installed authenticated Claude CLI and model quota"]
    async fn live_claude_same_run_steering() {
        live_cli(Protocol::Claude).await;
    }
}
