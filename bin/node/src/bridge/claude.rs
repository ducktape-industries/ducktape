//! The Claude Code adapter: `claude -p` speaking stream-json both ways, one child per place while
//! it has input, resumed by session id, its transcripts under CLAUDE_CONFIG_DIR and its ducktape
//! tools the bridge's own MCP relay.
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

use futures::channel::oneshot;
use futures::stream::{self, LocalBoxStream, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::adapter::{Adapter, Event, Turn};
use super::kept::Where;

pub(crate) const PASSWORD: &str = "DUCKTAPE_PASSWORD";
pub(crate) const CONFIG: &str = "CLAUDE_CONFIG_DIR";

pub(crate) struct Claude {
    pub(crate) program: PathBuf,
    pub(crate) exe: PathBuf,
    pub(crate) socket: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: Vec<(OsString, OsString)>,
}

pub(crate) struct Session {
    stdin: Option<ChildStdin>,
    kill: Option<oneshot::Sender<()>>,
}

impl Claude {
    pub(crate) fn command(&self, at: &Where, resume: Option<&str>) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--strict-mcp-config",
                "--allowedTools=mcp__ducktape",
            ])
            .arg("--mcp-config")
            .arg(self.mcp(at).to_string())
            .arg("--append-system-prompt")
            .arg(prompt(at));
        if let Some(session) = resume {
            command.arg("--resume").arg(session);
        }
        command.args(&self.args).env_clear().envs(self.env());
        command
    }

    pub(crate) fn env(&self) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = self
            .env
            .iter()
            .filter(|(name, _)| name != PASSWORD)
            .cloned()
            .collect();
        if !env.iter().any(|(name, _)| name == CONFIG) {
            env.push((CONFIG.into(), self.config.clone().into_os_string()));
        }
        env
    }

    fn mcp(&self, at: &Where) -> Value {
        json!({ "mcpServers": { "ducktape": {
            "type": "stdio",
            "command": self.exe.to_string_lossy(),
            "args": ["bridge", "relay", self.socket.to_string_lossy(), at.key()],
        } } })
    }
}

fn prompt(at: &Where) -> String {
    format!(
        "You answer on a ducktape network in thread {} of chat channel {}. Each message you are given is one heard there, or a wake you armed. When a turn ends, your final reply is posted in that thread as your account, so do not post it yourself. The ducktape tools act as that account.",
        at.thread, at.channel
    )
}

impl Adapter for Claude {
    type Turn = Session;

    fn start(
        &self,
        at: &Where,
        resume: Option<&str>,
    ) -> Result<(Session, LocalBoxStream<'static, Event>), String> {
        let mut child = self
            .command(at, resume)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|fault| format!("{} does not start: {fault}", self.program.display()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("claude's stdout is not piped")?;
        let (kill, killed) = oneshot::channel();
        let session = Session {
            stdin,
            kill: Some(kill),
        };
        Ok((session, events(child, stdout, killed)))
    }
}

impl Turn for Session {
    async fn say(&mut self, text: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("claude's input is closed")?;
        let mut line = user(text).to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|fault| fault.to_string())?;
        stdin.flush().await.map_err(|fault| fault.to_string())
    }

    fn finish(&mut self) {
        self.stdin = None;
    }

    fn stop(&mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }
}

pub(crate) fn user(text: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        "parent_tool_use_id": null,
        "session_id": "default",
    })
}

fn events(
    child: Child,
    stdout: ChildStdout,
    killed: oneshot::Receiver<()>,
) -> LocalBoxStream<'static, Event> {
    let lines = BufReader::new(stdout).lines();
    stream::unfold(Some((child, lines, killed)), |running| async move {
        let (mut child, mut lines, mut killed) = running?;
        let line = tokio::select! {
            line = lines.next_line() => line.ok().flatten(),
            _ = &mut killed => None,
        };
        match line {
            Some(line) => Some((parse(&line), Some((child, lines, killed)))),
            None => {
                let _ = child.start_kill();
                let why = match child.wait().await {
                    Ok(status) => status.to_string(),
                    Err(fault) => fault.to_string(),
                };
                Some((vec![Event::Ended(why)], None))
            }
        }
    })
    .flat_map(stream::iter)
    .boxed_local()
}

pub(crate) fn parse(line: &str) -> Vec<Event> {
    let Ok(said) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    if !said["parent_tool_use_id"].is_null() {
        return Vec::new();
    }
    match said["type"].as_str() {
        Some("system") if said["subtype"] == json!("init") => said["session_id"]
            .as_str()
            .map(|session| Event::Session(session.to_owned()))
            .into_iter()
            .collect(),
        Some("assistant") => said["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().map(|text| Event::Text(text.into())),
                Some("tool_use") => block["name"].as_str().map(|name| Event::Doing(name.into())),
                _ => None,
            })
            .collect(),
        Some("result") => {
            let text = match said["result"].as_str() {
                Some(text) => text.to_owned(),
                None => format!(
                    "claude stopped: {}",
                    said["subtype"].as_str().unwrap_or("with no result")
                ),
            };
            vec![Event::Answer(text)]
        }
        _ => Vec::new(),
    }
}
