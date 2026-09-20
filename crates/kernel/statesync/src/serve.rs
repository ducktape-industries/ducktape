use std::collections::BTreeMap;

use abi::{BlobId, Refusal, reason};
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_consensus::types::Height;
use commonware_storage::qmdb::sync::Source as _;
use consensus::MarshalMailbox;
use host::Tip;
use node::Node;

use crate::wire::{Anchor, Head, Request, Response};
use crate::{Context, SyncRequest};

pub trait Anchors: Send + Sync {
    fn anchor(&self, tip: Tip) -> impl Future<Output = Option<Anchor>> + Send;
}

impl Anchors for MarshalMailbox {
    async fn anchor(&self, tip: Tip) -> Option<Anchor> {
        if tip.height == 0 {
            return self.get_block(Height::zero()).await.map(Anchor::Genesis);
        }
        self.get_finalization(Height::new(tip.height))
            .await
            .map(Anchor::Finalized)
    }
}

pub async fn serve<E: Context, A: Anchors>(
    node: &Node<E>,
    anchors: &A,
    request: Request,
) -> Response {
    match request {
        Request::Head => head(node, anchors).await,
        Request::Sync { program, request } => sync(node, &program, &request).await,
        Request::Blob(id) => blob(node, &id),
    }
}

async fn head<E: Context, A: Anchors>(node: &Node<E>, anchors: &A) -> Response {
    let tip = match node.tip() {
        Ok(tip) => tip,
        Err(error) => return refused(reason::NOT_FOUND, error),
    };
    let Some(anchor) = anchors.anchor(tip).await else {
        return refused(reason::NOT_FOUND, "the tip has no anchor to start from");
    };
    let mut targets = BTreeMap::new();
    for (program, commitment) in node.host().store().commitments() {
        match commitment.target() {
            Ok(Some(target)) => {
                targets.insert(program.clone(), target);
            }
            Ok(None) => {
                return refused(reason::NOT_FOUND, format!("{program} has nothing to sync"));
            }
            Err(error) => return refused(reason::NOT_FOUND, error),
        }
    }
    Response::Head(Head {
        tip,
        anchor,
        targets,
    })
}

async fn sync<E: Context>(node: &Node<E>, program: &str, request: &[u8]) -> Response {
    let Some(commitment) = node.host().store().commitment(program) else {
        return refused(
            reason::UNKNOWN_PROGRAM,
            "the program is not on this network",
        );
    };
    let request = match SyncRequest::decode(request) {
        Ok(request) => request,
        Err(error) => return refused(reason::PROTOCOL, error),
    };
    let db = match commitment.db() {
        Ok(db) => db,
        Err(error) => return refused(reason::NOT_FOUND, error),
    };
    match db.serve(request).await {
        Ok((response, _)) => Response::Sync(response.encode().to_vec()),
        Err(error) => refused(reason::INVALID_INPUT, error),
    }
}

fn blob<E: Context>(node: &Node<E>, id: &BlobId) -> Response {
    match node.host().blob(id) {
        Ok(framed) => Response::Blob(framed),
        Err(error) => refused(reason::NOT_FOUND, error),
    }
}

fn refused(reason: &str, sentence: impl ToString) -> Response {
    Response::Refused(Refusal::new(reason, sentence.to_string()))
}
