//! The MCP relay between a product session and its bridge: the bridge serves `ducktape mcp` on a
//! socket in a folder only its user may open, each session's relay names the place it speaks for,
//! and a wake that session arms is told back to that place.
use std::fs::{DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use abi::BlobId;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use futures::stream::FuturesUnordered;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::WAKE;
use crate::Sentence as _;
use crate::json::Code;
use crate::mcp::{self, Landed, Network};

pub(crate) fn socket(state: &Path) -> Result<PathBuf, String> {
    let mut folder = state.as_os_str().to_owned();
    folder.push(".relay");
    let folder = PathBuf::from(folder);
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .and_then(|()| std::fs::set_permissions(&folder, Permissions::from_mode(0o700)))
        .map_err(|fault| format!("the relay folder {} is not made: {fault}", folder.display()))?;
    let socket = folder.join("mcp.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => Ok(socket),
        Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(socket),
        Err(fault) => Err(format!(
            "the old socket {} stays: {fault}",
            socket.display()
        )),
    }
}

pub(crate) async fn serve<N: Network>(
    listener: &UnixListener,
    network: &N,
    armed: UnboundedSender<(u64, String)>,
) -> Result<(), String> {
    let mut sessions = FuturesUnordered::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.sentence()?;
                sessions.push(session(stream, network, armed.clone()));
            }
            Some(ended) = sessions.next() => {
                if let Err(fault) = ended {
                    eprintln!("bridge: a relayed MCP session ended: {fault}");
                }
            }
        }
    }
}

async fn session<N: Network>(
    stream: UnixStream,
    network: &N,
    armed: UnboundedSender<(u64, String)>,
) -> Result<(), String> {
    let (read, write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut place = String::new();
    read.read_line(&mut place).await.sentence()?;
    let relayed = Relayed {
        network,
        place: place.trim_end().to_owned(),
        armed,
    };
    mcp::serve(&relayed, read, write).await.sentence()
}

struct Relayed<'a, N> {
    network: &'a N,
    place: String,
    armed: UnboundedSender<(u64, String)>,
}

impl<N: Network> Network for Relayed<'_, N> {
    async fn whom(&self) -> String {
        self.network.whom().await
    }

    async fn running(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<(String, Result<Code, String>)>, String> {
        self.network.running(only).await
    }

    async fn query(&self, program: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        self.network.query(program, request).await
    }

    async fn send(&self, program: &str, payload: Vec<u8>) -> Result<Landed, String> {
        self.network.send(program, payload).await
    }

    async fn egress(&self) -> Result<Option<String>, String> {
        self.network.egress().await
    }

    async fn put(&self, kind: &str, bodies: Vec<Vec<u8>>) -> Result<Vec<BlobId>, String> {
        self.network.put(kind, bodies).await
    }

    async fn landed(&self, program: &str, op: &Value, answer: &Value) {
        if let Some(wake) = armed(program, op, answer) {
            let _ = self.armed.unbounded_send((wake, self.place.clone()));
        }
    }
}

pub(crate) fn armed(program: &str, op: &Value, answer: &Value) -> Option<u64> {
    if program != WAKE || op.get("wake_on").is_none() {
        return None;
    }
    answer["returned"].as_u64()
}

pub(crate) async fn relay(socket: PathBuf, place: String) -> Result<(), String> {
    let stream = UnixStream::connect(&socket)
        .await
        .map_err(|fault| format!("no bridge answers at {}: {fault}", socket.display()))?;
    let (mut read, mut write) = stream.into_split();
    write
        .write_all(format!("{place}\n").as_bytes())
        .await
        .sentence()?;
    let up = async {
        tokio::io::copy(&mut tokio::io::stdin(), &mut write).await?;
        write.shutdown().await
    };
    let mut stdout = tokio::io::stdout();
    let down = tokio::io::copy(&mut read, &mut stdout);
    tokio::pin!(up, down);
    let mut sending = true;
    loop {
        tokio::select! {
            sent = &mut up, if sending => {
                sent.sentence()?;
                sending = false;
            }
            got = &mut down => {
                got.sentence()?;
                return Ok(());
            }
        }
    }
}
