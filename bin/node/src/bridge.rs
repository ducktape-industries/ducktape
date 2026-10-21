//! `ducktape bridge`: an agent product answering as an agent account. It hears what attribution
//! tells the account, runs one product session per chat thread, drafts and posts each answer, and
//! serves the session's ducktape tools itself over a socket, so no child holds the wallet key.
mod adapter;
mod chain;
mod claude;
mod heard;
mod kept;
mod relay;
mod run;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use commonware_cryptography::ed25519;
use noded::{Client, Workspace};

pub(crate) use relay::relay;

use crate::Sentence as _;
use chain::Chain as _;

const ATTRIBUTION: &str = "attribution";
const CHAT: &str = "chat";
const WAKE: &str = "wake";

pub(crate) struct Asked {
    pub(crate) state: PathBuf,
    pub(crate) claude: PathBuf,
    pub(crate) args: Vec<String>,
}

pub(crate) fn key(
    workspace: &Workspace,
    password_stdin: bool,
) -> Result<ed25519::PrivateKey, String> {
    if !password_stdin {
        return crate::signer(workspace);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).sentence()?;
    let password = line.trim_end_matches(['\r', '\n']);
    let path = keystore::wallet::active_user_key(workspace.dir())?;
    keystore::userkey::open_user_key_at(&path, password)
}

pub(crate) async fn claude(
    workspace: &Workspace,
    client: &Client,
    key: ed25519::PrivateKey,
    asked: Asked,
) -> Result<(), String> {
    let live = chain::Live::seat(workspace, client, key).await?;
    let socket = relay::socket(&asked.state)?;
    let listener = tokio::net::UnixListener::bind(&socket).map_err(|fault| {
        format!(
            "the relay socket {} does not open: {fault}",
            socket.display()
        )
    })?;
    let mut config = asked.state.as_os_str().to_owned();
    config.push(".claude");
    let adapter = claude::Claude {
        program: asked.claude,
        exe: std::env::current_exe().sentence()?,
        socket,
        config: PathBuf::from(config),
        args: asked.args,
        env: std::env::vars_os().collect(),
    };
    let mut bridge = run::Bridge::open(&live, &adapter, &asked.state)?;
    let (armed, heard) = futures::channel::mpsc::unbounded();
    let stop = crate::signalled().sentence()?;
    let follow = live.follow().await?;
    tokio::select! {
        ran = bridge.run(follow, heard, stop) => ran,
        served = relay::serve(&listener, &live.mcp, armed) => served,
    }
}
