//! GET /v1/release — what this node says about the release plane: where the
//! chain is, the node release the network designates, and the key each kind
//! of release is signed with ([`app_update::ReleaseStatus`]).
//!
//! It is the whole read `ducktape release status` makes, so the node launcher
//! and the desktop app decode one document: the launcher through that verb,
//! the app — which cannot run the CLI — straight off this route. Read only,
//! no credential: every field is committed state or the status snapshot
//! `/v1/status` already serves to anyone.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures::channel::oneshot;

use app_update::{ReleaseKeys, ReleaseSignal, ReleaseStatus};

use crate::{NodeCommand, NodeHandle};

/// How far one walk goes. A network that has passed this many decisions in
/// one id space has outgrown a linear probe, not this plane.
const MAX_PROPOSALS: u64 = 1024;

pub(crate) async fn release(State(handle): State<NodeHandle>) -> Response {
    let live = handle.status_cell().current();
    let signals = passed_signals(&handle, app_update::designation::proposal_id).await;
    let designation = app_update::designation::standing(
        signals
            .iter()
            .filter_map(|text| ReleaseSignal::from_signal_text(text)),
    )
    .pop();
    let keys = passed_signals(&handle, app_update::release_key::proposal_id).await;
    let release_keys = ReleaseKeys::committed(keys.iter().map(String::as_str));
    Json(ReleaseStatus {
        base: String::new(),
        public_key: live.public_key,
        height: live.height,
        root_hash: live.root_hash,
        designation,
        release_keys,
    })
    .into_response()
}

/// The text of every PASSED `Signal` in one keyless id space, in id order.
/// The walk stops at the first id with no record, which is exactly where the
/// ceremony's own mint stops.
///
/// A node that is up but whose governance cannot answer yet is still a node
/// its launcher must hear about — the identity half of the document matters
/// first — so a walk that fails reads as "nothing decided" rather than failing
/// the route.
async fn passed_signals(handle: &NodeHandle, id: fn(u64) -> String) -> Vec<String> {
    let mut passed = Vec::new();
    for nth in 0..MAX_PROPOSALS {
        let view = match proposal(handle, id(nth)).await {
            Ok(Some(view)) => view,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(
                    target: "ducktape::update",
                    reason = "governance_unreadable",
                    %error,
                    "the release plane's governance walk did not answer"
                );
                return Vec::new();
            }
        };
        let decided = view.status == governance::ProposalStatus::Passed;
        let governance::GovAction::Signal { text } = view.action else {
            continue;
        };
        if decided {
            passed.push(text);
        }
    }
    passed
}

async fn proposal(
    handle: &NodeHandle,
    proposal_id: String,
) -> Result<Option<governance::ProposalView>, String> {
    let (reply, rx) = oneshot::channel();
    handle
        .send(NodeCommand::Query {
            target: "governance".into(),
            req: governance::encode_query(&governance::GovQuery::Proposal { proposal_id }),
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx
        .await
        .map_err(|_| "reply dropped".to_string())?
        .map_err(|refused| refused.message)?;
    match governance::decode_reply(&bytes)? {
        governance::GovReply::Proposal(view) => Ok(view),
        other => Err(format!("unexpected governance reply: {other:?}")),
    }
}
