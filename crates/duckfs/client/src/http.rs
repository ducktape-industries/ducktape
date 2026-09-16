//! Files client over the generic module query and signed-frame submit lanes.
//! Product request and reply shapes belong to this client and the Files guest;
//! the node transports opaque module messages. Every write retains its signer.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_core::objects::object_id;
use duckfs_core::{
    Change, DiffEntry, DigestHex, EntryInfo, FilesMsg, FilesQuery, FilesReply, Kind, RefsInfo,
    SnapshotInfo, encode_msg, encode_putblob, to_hex,
};
use serde::Deserialize;

use crate::api::{ApiError, CommitReceipt, NodeApi};

/// Sign an arbitrary module operation as a consensus frame. The caller owns
/// the key and sequence; the client sends the returned bytes unchanged.
pub type FrameSigner = Arc<dyn Fn(&str, Vec<u8>) -> Vec<u8> + Send + Sync>;

/// Credential of the node operator, read afresh for each node-authored op.
pub type OperatorCredential = Arc<dyn Fn() -> Option<String> + Send + Sync>;

enum WriteAuthority {
    SignedFrame(FrameSigner),
    NodeOperator(OperatorCredential),
}

pub struct HttpNode {
    client: reqwest::blocking::Client,
    base: String,
    writer: Option<WriteAuthority>,
}

impl HttpNode {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("build a blocking reqwest client");
        Self {
            client,
            base: base_url.into().trim_end_matches('/').to_string(),
            writer: None,
        }
    }

    pub fn with_frame_signer(mut self, signer: FrameSigner) -> Self {
        self.writer = Some(WriteAuthority::SignedFrame(signer));
        self
    }

    /// Local operator processes act as the node; this never impersonates a user.
    pub fn with_operator_credential(mut self, credential: OperatorCredential) -> Self {
        self.writer = Some(WriteAuthority::NodeOperator(credential));
        self
    }

    fn run(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response, ApiError> {
        let response = request
            .send()
            .map_err(|error| ApiError::Transport(error.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        if status.as_u16() == 404 {
            return Err(ApiError::NotFound);
        }
        let text = response.text().unwrap_or_default();
        let message = serde_json::from_str::<ErrorBody>(&text)
            .map(|body| body.error)
            .unwrap_or(text);
        Err(ApiError::Rejected(message))
    }

    fn query(&self, query: FilesQuery) -> Result<FilesReply, ApiError> {
        self.run(
            self.client
                .post(format!("{}/v1/query", self.base))
                .json(&serde_json::json!({ "target": "files", "query": query })),
        )?
        .json()
        .map_err(|error| ApiError::Transport(error.to_string()))
    }

    fn submit(&self, payload: Vec<u8>) -> Result<CommitReceipt, ApiError> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| ApiError::Rejected("a write authority is required".into()))?;
        let request = match writer {
            WriteAuthority::SignedFrame(signer) => self
                .client
                .post(format!("{}/v1/submit/frame", self.base))
                .body(signer("files", payload)),
            WriteAuthority::NodeOperator(credential) => {
                let token = credential().ok_or_else(|| {
                    ApiError::Rejected("node operator credential unavailable".into())
                })?;
                self.client
                    .post(format!("{}/v1/submit/raw/files", self.base))
                    .header("x-ducktape-admin-token", token)
                    .body(payload)
            }
        };
        self.run(request.header("content-type", "application/octet-stream"))?
            .json()
            .map_err(|error| ApiError::Transport(error.to_string()))
    }
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

fn unexpected() -> ApiError {
    ApiError::Transport("unexpected Files module reply".into())
}

impl NodeApi for HttpNode {
    fn refs(&self) -> Result<RefsInfo, ApiError> {
        match self.query(FilesQuery::Refs {})? {
            FilesReply::Refs(refs) => Ok(refs),
            _ => Err(unexpected()),
        }
    }

    fn stat(&self, path: &str, snapshot: Option<&str>) -> Result<Option<EntryInfo>, ApiError> {
        match self.query(FilesQuery::Stat {
            path: path.into(),
            snapshot: snapshot.map(str::to_owned),
        })? {
            FilesReply::Stat(entry) => Ok(entry),
            _ => Err(unexpected()),
        }
    }

    fn ls(
        &self,
        path: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        match self.query(FilesQuery::Ls {
            path: path.into(),
            snapshot: snapshot.map(str::to_owned),
            after: after.map(str::to_owned),
            limit,
        })? {
            FilesReply::Ls { entries, next } => Ok((entries, next)),
            _ => Err(unexpected()),
        }
    }

    fn find(
        &self,
        prefix: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        match self.query(FilesQuery::Find {
            prefix: prefix.into(),
            snapshot: snapshot.map(str::to_owned),
            after: after.map(str::to_owned),
            limit,
        })? {
            FilesReply::Find { entries, next } => Ok((entries, next)),
            _ => Err(unexpected()),
        }
    }

    fn read(
        &self,
        path: &str,
        snapshot: Option<&str>,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, bool), ApiError> {
        match self.query(FilesQuery::Read {
            path: path.into(),
            snapshot: snapshot.map(str::to_owned),
            offset,
            len,
        })? {
            FilesReply::Read { b64, eof } => STANDARD
                .decode(b64.as_bytes())
                .map(|bytes| (bytes, eof))
                .map_err(|error| ApiError::Transport(error.to_string())),
            _ => Err(unexpected()),
        }
    }

    fn history(&self, limit: u64) -> Result<Vec<SnapshotInfo>, ApiError> {
        match self.query(FilesQuery::History { limit })? {
            FilesReply::History(snapshots) => Ok(snapshots),
            _ => Err(unexpected()),
        }
    }

    fn diff(&self, from: &str, to: &str, prefix: &str) -> Result<Vec<DiffEntry>, ApiError> {
        match self.query(FilesQuery::Diff {
            from: from.into(),
            to: to.into(),
            prefix: prefix.into(),
        })? {
            FilesReply::Diff(entries) => Ok(entries),
            _ => Err(unexpected()),
        }
    }

    fn has_chunks(&self, ids: &[String]) -> Result<Vec<bool>, ApiError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        match self.query(FilesQuery::HasChunks { ids: ids.to_vec() })? {
            FilesReply::HasChunks { present } => Ok(present),
            _ => Err(unexpected()),
        }
    }

    fn stage_chunk(&self, bytes: &[u8]) -> Result<DigestHex, ApiError> {
        self.submit(encode_putblob(bytes))?;
        Ok(to_hex(&object_id(Kind::Chunk, bytes)))
    }

    fn commit(
        &self,
        base: Option<&str>,
        message: &str,
        changes: Vec<Change>,
    ) -> Result<CommitReceipt, ApiError> {
        self.submit(encode_msg(&FilesMsg::Commit {
            base_snapshot: base.map(str::to_owned),
            message: message.into(),
            changes,
        }))
    }

    fn pin(&self, snapshot: &str, name: &str) -> Result<(), ApiError> {
        self.submit(encode_msg(&FilesMsg::Pin {
            snapshot: snapshot.into(),
            name: name.into(),
        }))?;
        Ok(())
    }

    fn unpin(&self, name: &str) -> Result<(), ApiError> {
        self.submit(encode_msg(&FilesMsg::Unpin { name: name.into() }))?;
        Ok(())
    }
}
