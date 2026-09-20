//! `ActorNodeApi`: the checkout/commit engine's [`NodeApi`] over the in-daemon
//! `NodeCommand` actor lane — no self-dial.
//!
//! the workspace RPC (`workspaces.rs`) runs the same `duckfs-client` engine the
//! CLI does, but a daemon talking to its OWN http surface would deadlock the
//! single actor. instead this adapter encodes the duckfs wire (a putblob frame
//! or a `FilesMsg`/`FilesQuery`) and threads it straight onto the actor lane the
//! http handlers use, `futures::executor::block_on`-ing the futures mpsc send +
//! oneshot reply. that is safe ONLY on a `spawn_blocking` thread (never an axum
//! worker): the actor lives on its own thread, and futures channels are
//! executor-agnostic, so blocking one blocking-pool thread on the reply never
//! starves the actor. a module rejection returns as [`ApiError::Rejected`] with
//! both halves untouched — its class beside its verbatim sentence, which the
//! conflict taxonomy still keys on.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use duckfs_client::api::{ApiError, CommitReceipt, NodeApi};
use duckfs_core::{
    Change, DiffEntry, DigestHex, EntryInfo, FilesMsg, FilesQuery, FilesReply, RefsInfo,
    SnapshotInfo, decode_reply, encode_msg, encode_putblob, encode_query,
};
use futures::channel::oneshot;

const FILES_MODULE: &str = "files";
use crate::{BlockSummary, NodeCommand, NodeHandle, Refused};

/// a `NodeApi` bound to one node's actor lane. cheap to clone (holds only the
/// command-channel handle); a fresh one is made per workspace request.
pub(crate) struct ActorNodeApi {
    handle: NodeHandle,
    /// the ACTING identity every op this adapter submits is authored by — the
    /// key the workspace request's signature proved possession of. carried
    /// rather than defaulted so a managed checkout's commits are charged to the
    /// person who asked for them, not to the daemon.
    origin: Vec<u8>,
}

impl ActorNodeApi {
    pub(crate) fn new(handle: NodeHandle, origin: Vec<u8>) -> Self {
        ActorNodeApi { handle, origin }
    }

    /// submit an already-encoded files op (putblob frame or `FilesMsg`) as one
    /// block over the actor lane, authored by the acting key.
    fn submit(&self, payload: Vec<u8>) -> Result<BlockSummary, ApiError> {
        futures::executor::block_on(async {
            let (reply, rx) = oneshot::channel();
            self.handle
                .send(NodeCommand::Submit {
                    required_blob: None,
                    target: FILES_MODULE.into(),
                    payload,
                    origin: self.origin.clone(),
                    reply,
                })
                .await
                .map_err(|_| ApiError::Transport("node actor is gone".into()))?;
            match rx.await {
                Ok(Ok(block)) => Ok(block),
                // the module's own class AND sentence pass through untouched.
                Ok(Err(refused)) => Err(ApiError::Rejected {
                    reason: refused.reason,
                    sentence: refused.message,
                }),
                Err(_) => Err(ApiError::Transport("node actor dropped the reply".into())),
            }
        })
    }

    /// run a typed files query over the actor lane and decode its reply.
    fn query(&self, q: &FilesQuery) -> Result<FilesReply, ApiError> {
        futures::executor::block_on(async {
            let (reply, rx) = oneshot::channel();
            self.handle
                .send(NodeCommand::Query {
                    target: FILES_MODULE.into(),
                    req: encode_query(q),
                    reply,
                })
                .await
                .map_err(|_| ApiError::Transport("node actor is gone".into()))?;
            let bytes = match rx.await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(refused)) => return Err(map_query_error(refused)),
                Err(_) => return Err(ApiError::Transport("node actor dropped the reply".into())),
            };
            decode_reply(&bytes).map_err(ApiError::Transport)
        })
    }
}

/// map a query rejection to the api taxonomy: an absent path or an unresolvable
/// snapshot is a 404-equivalent [`ApiError::NotFound`]; every other rejection
/// keeps both halves (the same mapping the http surface uses).
///
/// the absent/unresolvable test reads the SENTENCE because the files module
/// answers every query rejection under one `files_query` class — the class it
/// would take instead does not exist yet.
fn map_query_error(refused: Refused) -> ApiError {
    let absent =
        refused.message.contains("not found") || refused.message.contains("not resolvable");
    if absent {
        return ApiError::NotFound;
    }
    ApiError::Rejected {
        reason: refused.reason,
        sentence: refused.message,
    }
}

/// a reply variant the query never asks for is daemon/module wire drift.
fn wrong(reply: FilesReply) -> ApiError {
    ApiError::Transport(format!("unexpected files reply: {reply:?}"))
}

impl NodeApi for ActorNodeApi {
    fn refs(&self) -> Result<RefsInfo, ApiError> {
        match self.query(&FilesQuery::Refs {})? {
            FilesReply::Refs(info) => Ok(info),
            other => Err(wrong(other)),
        }
    }

    fn stat(&self, path: &str, snapshot: Option<&str>) -> Result<Option<EntryInfo>, ApiError> {
        match self.query(&FilesQuery::Stat {
            path: path.into(),
            snapshot: snapshot.map(Into::into),
        })? {
            FilesReply::Stat(e) => Ok(e),
            other => Err(wrong(other)),
        }
    }

    fn ls(
        &self,
        path: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        match self.query(&FilesQuery::Ls {
            path: path.into(),
            snapshot: snapshot.map(Into::into),
            after: after.map(Into::into),
            limit,
        })? {
            FilesReply::Ls { entries, next } => Ok((entries, next)),
            other => Err(wrong(other)),
        }
    }

    fn find(
        &self,
        prefix: &str,
        snapshot: Option<&str>,
        after: Option<&str>,
        limit: u64,
    ) -> Result<(Vec<EntryInfo>, Option<String>), ApiError> {
        match self.query(&FilesQuery::Find {
            prefix: prefix.into(),
            snapshot: snapshot.map(Into::into),
            after: after.map(Into::into),
            limit,
        })? {
            FilesReply::Find { entries, next } => Ok((entries, next)),
            other => Err(wrong(other)),
        }
    }

    fn read(
        &self,
        path: &str,
        snapshot: Option<&str>,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, bool), ApiError> {
        match self.query(&FilesQuery::Read {
            path: path.into(),
            snapshot: snapshot.map(Into::into),
            offset,
            len,
        })? {
            FilesReply::Read { b64, eof } => {
                let bytes = STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| ApiError::Transport(e.to_string()))?;
                Ok((bytes, eof))
            }
            other => Err(wrong(other)),
        }
    }

    fn history(&self, limit: u64) -> Result<Vec<SnapshotInfo>, ApiError> {
        match self.query(&FilesQuery::History { limit })? {
            FilesReply::History(snaps) => Ok(snaps),
            other => Err(wrong(other)),
        }
    }

    fn diff(&self, from: &str, to: &str, prefix: &str) -> Result<Vec<DiffEntry>, ApiError> {
        match self.query(&FilesQuery::Diff {
            from: from.into(),
            to: to.into(),
            prefix: prefix.into(),
        })? {
            FilesReply::Diff(entries) => Ok(entries),
            other => Err(wrong(other)),
        }
    }

    fn has_chunks(&self, ids: &[String]) -> Result<Vec<bool>, ApiError> {
        match self.query(&FilesQuery::HasChunks { ids: ids.to_vec() })? {
            FilesReply::HasChunks { present } => Ok(present),
            other => Err(wrong(other)),
        }
    }

    fn stage_chunk(&self, bytes: &[u8]) -> Result<DigestHex, ApiError> {
        self.submit(encode_putblob(bytes))?;
        Ok(duckfs_core::to_hex(&duckfs_core::objects::object_id(
            duckfs_core::Kind::Chunk,
            bytes,
        )))
    }

    fn commit(
        &self,
        base: Option<&str>,
        message: &str,
        changes: Vec<Change>,
    ) -> Result<CommitReceipt, ApiError> {
        let block = self.submit(encode_msg(&FilesMsg::Commit {
            base_snapshot: base.map(Into::into),
            message: message.into(),
            changes,
        }))?;
        Ok(CommitReceipt {
            height: block.height,
        })
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
