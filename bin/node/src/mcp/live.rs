//! The node `ducktape mcp` speaks to: each program's live code and the sections it carries, the
//! active wallet key's signed queries and ops, and chunk blobs put through the node's upload.
use abi::role::identity::{Query, Reply};
use abi::{BlobId, Outcome, role};
use commonware_cryptography::{Signer as _, ed25519};
use host::Layer;
use node::Frame;
use noded::wire::Status;
use noded::{Client, Workspace};

use super::{Landed, Network};
use crate::json::{self, Code};
use crate::{Sentence as _, salt};

pub struct Live<'a> {
    workspace: &'a Workspace,
    client: &'a Client,
    key: ed25519::PrivateKey,
    whom: String,
}

impl<'a> Live<'a> {
    pub async fn seat(
        workspace: &'a Workspace,
        client: &'a Client,
        key: ed25519::PrivateKey,
    ) -> Result<Live<'a>, String> {
        let mut live = Live {
            workspace,
            client,
            key,
            whom: String::new(),
        };
        live.whom = live.account().await?;
        Ok(live)
    }
}

impl Live<'_> {
    fn public(&self) -> String {
        hex::encode(self.key.public_key().as_ref())
    }

    fn signed(&self, status: &Status, program: &str, payload: Vec<u8>) -> Frame {
        Frame::sign(
            &self.key,
            status.network.as_bytes(),
            status.anchor(),
            Some(salt()),
            program,
            payload,
        )
    }

    async fn landed(&self, frame: &Frame) -> Result<Landed, String> {
        match self.client.land(frame).await {
            Ok((height, receipt)) => Ok(Landed {
                height,
                outcome: receipt.outcome,
            }),
            Err(noded::Error::Unlanded) => Err(format!(
                "unlanded: no block carried frame {} before its anchor aged out",
                hex::encode(frame.body.submission().id())
            )),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn identity(
        &self,
        status: &Status,
        program: &str,
        query: &Query,
    ) -> Result<Reply, String> {
        let request = abi::encode(query);
        let answer = json::ask(
            self.client,
            &self.key,
            status,
            program,
            request,
            Layer::Preconfirmed,
        )
        .await?;
        abi::decode(&answer).map_err(|refusal| refusal.sentence)
    }

    async fn account(&self) -> Result<String, String> {
        let status = self.client.status().await.sentence()?;
        let program = status
            .roles
            .bound(role::IDENTITY)
            .map_err(|refusal| refusal.sentence)?
            .to_string();
        let key = self.key.public_key().as_ref().to_vec();
        let Reply::Account(number) = self
            .identity(&status, &program, &Query::Account(key))
            .await?
        else {
            return Err(format!("program {program} answered no account"));
        };
        let profile = match number {
            None => None,
            Some(number) => match self
                .identity(&status, &program, &Query::Profile(number))
                .await?
            {
                Reply::Profile(profile) => profile,
                _ => return Err(format!("program {program} answered no profile")),
            },
        };
        let (profile, manager) = super::agent(&self.public(), profile)?;
        Ok(format!(
            "You act on network {} through the node at {} as agent account {} ({}), managed by account {manager}, with the key {}. Every op you send with network_call and every file you write with files_write is that account's, and each program decides what it may do. network_schema with no program lists the programs that run.",
            status.network,
            self.client.base(),
            profile.number,
            profile.name,
            self.public()
        ))
    }
}

impl Network for Live<'_> {
    async fn whom(&self) -> String {
        self.whom.clone()
    }

    async fn running(
        &self,
        only: Option<&str>,
    ) -> Result<Vec<(String, Result<Code, String>)>, String> {
        let mut programs = self.client.programs().await.sentence()?;
        let chosen: Vec<(String, BlobId)> = match only {
            None => programs.into_iter().collect(),
            Some(program) => programs.remove_entry(program).into_iter().collect(),
        };
        json::codes(self.workspace, self.client, chosen).await
    }

    async fn query(&self, program: &str, request: Vec<u8>) -> Result<Vec<u8>, String> {
        let status = self.client.status().await.sentence()?;
        json::ask(
            self.client,
            &self.key,
            &status,
            program,
            request,
            Layer::Preconfirmed,
        )
        .await
    }

    async fn send(&self, program: &str, payload: Vec<u8>) -> Result<Landed, String> {
        let status = self.client.status().await.sentence()?;
        let frame = self.signed(&status, program, payload);
        self.landed(&frame).await
    }

    async fn egress(&self) -> Result<Option<String>, String> {
        let status = self.client.status().await.sentence()?;
        Ok(status.roles.bound(role::EGRESS).ok().cloned())
    }

    async fn put(&self, kind: &str, bodies: Vec<Vec<u8>>) -> Result<Vec<BlobId>, String> {
        let framed = bodies
            .iter()
            .map(|body| blob::frame(kind, body))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|refusal| refusal.sentence)?;
        let entries = framed
            .iter()
            .map(|framed| blob::contract::Entry::of(framed))
            .collect::<Option<Vec<_>>>()
            .ok_or("a framed chunk does not read as a blob")?;
        let ids: Vec<BlobId> = entries.iter().map(|entry| entry.id).collect();
        let status = self.client.status().await.sentence()?;
        let role = status
            .roles
            .bound(role::BLOBS)
            .map_err(|refusal| refusal.sentence)?
            .to_string();
        let put = blob::contract::Op::Put { blobs: entries };
        let frame = self.signed(&status, &role, abi::encode(&put));
        let (height, receipt) = self.client.land_framed(&frame, framed).await.sentence()?;
        let output = match receipt.outcome {
            Outcome::Applied { output } => output,
            Outcome::Rejected(refusal) => {
                return Err(format!(
                    "the chunks' put was rejected at block {height}: {}: {}",
                    refusal.reason, refusal.sentence
                ));
            }
        };
        let bundle: u64 = abi::decode(&output).map_err(|refusal| refusal.sentence)?;
        self.client.held(&role, bundle).await.sentence()?;
        Ok(ids)
    }
}
