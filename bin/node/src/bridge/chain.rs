//! The network as the bridge speaks to it: each program in JSON through the schema its live code
//! carries, as `ducktape op` and `ducktape query` speak it, and the seqs of the account's own
//! attribution rows as they land.
use abi::{Outcome, role};
use borsh_json::spoken;
use commonware_cryptography::ed25519;
use futures::stream::{self, LocalBoxStream, StreamExt as _};
use noded::wire::Change;
use noded::{Client, Workspace};
use serde_json::Value;

use super::ATTRIBUTION;
use crate::mcp::{self, Network as _};
use crate::{Sentence as _, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Sent {
    Applied(Value),
    Rejected { reason: String, sentence: String },
}

pub(crate) trait Chain {
    fn account(&self) -> u64;
    fn identity(&self) -> &str;
    async fn query(&self, program: &str, query: Value) -> Result<Value, String>;
    async fn send(&self, program: &str, op: Value) -> Result<Sent, String>;
    async fn follow(&self) -> Result<LocalBoxStream<'_, Result<u64, String>>, String>;
}

pub(crate) struct Live<'a> {
    pub(crate) mcp: mcp::Live<'a>,
    workspace: &'a Workspace,
    client: &'a Client,
    identity: String,
}

impl<'a> Live<'a> {
    pub(crate) async fn seat(
        workspace: &'a Workspace,
        client: &'a Client,
        key: ed25519::PrivateKey,
    ) -> Result<Live<'a>, String> {
        let status = client.status().await.sentence()?;
        let identity = status
            .roles
            .bound(role::IDENTITY)
            .map_err(|refusal| refusal.sentence)?
            .to_string();
        let mcp = mcp::Live::seat(workspace, client, key).await?;
        Ok(Live {
            mcp,
            workspace,
            client,
            identity,
        })
    }
}

impl Chain for Live<'_> {
    fn account(&self) -> u64 {
        self.mcp.number()
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    async fn query(&self, program: &str, query: Value) -> Result<Value, String> {
        let interface = json::interface(self.workspace, self.client, program).await?;
        let request = interface
            .encode_query(&spoken(&query))
            .map_err(|fault| fault.to_string())?;
        let answer = self.mcp.query(program, request).await?;
        json::reply(&interface, program, &answer)
    }

    async fn send(&self, program: &str, op: Value) -> Result<Sent, String> {
        let interface = json::interface(self.workspace, self.client, program).await?;
        let payload = interface
            .encode_op(&spoken(&op))
            .map_err(|fault| fault.to_string())?;
        let landed = self.mcp.send(program, payload.clone()).await?;
        Ok(match landed.outcome {
            Outcome::Applied { output } => {
                let told = interface.returned(program, &payload, &output);
                Sent::Applied(told["returned"].clone())
            }
            Outcome::Rejected(refusal) => Sent::Rejected {
                reason: refusal.reason.to_string(),
                sentence: refusal.sentence,
            },
        })
    }

    async fn follow(&self) -> Result<LocalBoxStream<'_, Result<u64, String>>, String> {
        let changes = self.client.changes(ATTRIBUTION).await.sentence()?;
        let prefix = to_prefix(self.account());
        Ok(changes
            .flat_map(move |change| stream::iter(seqs(&prefix, change)))
            .boxed_local())
    }
}

pub(crate) fn to_prefix(account: u64) -> Vec<u8> {
    [b"to/".as_slice(), &account.to_be_bytes()].concat()
}

fn seqs<E: std::fmt::Display>(
    prefix: &[u8],
    change: Result<Change, E>,
) -> Vec<Result<u64, String>> {
    match change {
        Err(fault) => vec![Err(fault.to_string())],
        Ok(change) => change
            .writes
            .into_iter()
            .filter_map(|(key, value)| seq_of(prefix, &key, value.is_some()))
            .map(Ok)
            .collect(),
    }
}

pub(crate) fn seq_of(prefix: &[u8], key: &[u8], written: bool) -> Option<u64> {
    let seq: [u8; 8] = key.strip_prefix(prefix)?.try_into().ok()?;
    written.then(|| u64::from_be_bytes(seq))
}
