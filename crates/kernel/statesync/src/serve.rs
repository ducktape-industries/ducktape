use std::collections::BTreeMap;

use abi::{BlobId, ProgramId, Refusal, reason};
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_consensus::types::Height;
use commonware_storage::qmdb::sync::Source as _;
use consensus::MarshalMailbox;
use host::Tip;
use node::Node;

use crate::Anchor;
use crate::wire::{Head, Request, Response};
use crate::{Context, SyncRequest, SyncTarget};

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

pub enum Served {
    Answered(Response),
    Unanchored {
        tip: Tip,
        targets: Result<BTreeMap<ProgramId, SyncTarget>, Refusal>,
    },
}

pub async fn serve<E: Context>(node: &Node<E>, request: Request) -> Served {
    match request {
        Request::Head => head(node),
        Request::Sync { program, request } => {
            Served::Answered(sync(node, &program, &request).await)
        }
        Request::Blob(id) => Served::Answered(blob(node, &id).await),
    }
}

pub async fn anchored<A: Anchors>(served: Served, anchors: &A) -> Response {
    let (tip, targets) = match served {
        Served::Answered(response) => return response,
        Served::Unanchored { tip, targets } => (tip, targets),
    };
    let Some(anchor) = anchors.anchor(tip).await else {
        return refused(reason::NOT_FOUND, "the tip has no anchor to start from");
    };
    match targets {
        Ok(targets) => Response::Head(Head {
            tip,
            anchor,
            targets,
        }),
        Err(refusal) => Response::Refused(refusal),
    }
}

fn head<E: Context>(node: &Node<E>) -> Served {
    let tip = match node.tip() {
        Ok(tip) => tip,
        Err(error) => return Served::Answered(refused(reason::NOT_FOUND, error)),
    };
    Served::Unanchored {
        tip,
        targets: targets(node),
    }
}

fn targets<E: Context>(node: &Node<E>) -> Result<BTreeMap<ProgramId, SyncTarget>, Refusal> {
    let mut targets = BTreeMap::new();
    for (program, commitment) in node.host().store().commitments() {
        match commitment.target() {
            Ok(Some(target)) => {
                targets.insert(program.clone(), target);
            }
            Ok(None) => {
                let sentence = format!("{program} has nothing to sync");
                return Err(Refusal::new(reason::NOT_FOUND, sentence));
            }
            Err(error) => return Err(Refusal::new(reason::NOT_FOUND, error.to_string())),
        }
    }
    Ok(targets)
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
    let retained = db.bounds().start;
    let pruned_past = request.start() < retained;
    if pruned_past {
        tracing::debug!(
            target: "ducktape::sync",
            event = "sync_refused",
            program,
            reason = "pruned",
            start = *request.start(),
            retained = *retained,
        );
        return refused(
            reason::STALE,
            "this node pruned the operations the request starts at; read the head again",
        );
    }
    match db.serve(request).await {
        Ok((response, _)) => Response::Sync(response.encode().to_vec()),
        Err(error) => refused(reason::INVALID_INPUT, error),
    }
}

async fn blob<E: Context>(node: &Node<E>, id: &BlobId) -> Response {
    match node.host().blob(id, node.now()).await {
        Ok(framed) => Response::Blob(framed),
        Err(error) => refused(reason::NOT_FOUND, error),
    }
}

fn refused(reason: &str, sentence: impl ToString) -> Response {
    Response::Refused(Refusal::new(reason, sentence.to_string()))
}
