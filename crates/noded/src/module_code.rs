//! the module-code staging lane: the operator end of wasm code distribution.
//!
//! `POST /v1/admin/module-code/stage` ingests a canonical module artifact
//! (a Wasm component and optional mapper) into this node's content-addressed blob
//! store and — unless `?fanout=false` — hands the digest to the node's
//! code-plane fan-out, which pushes the bytes to every member and collects
//! per-peer receipts. the response is the digest (what a governance
//! `UpdateModule` proposal commits) plus those receipts, so the operator
//! submits the proposal only once the network demonstrably holds the bytes.
//! `GET /v1/admin/module-code/{digest}` reports local residency.
//!
//! the daemon owns only the lane's HTTP end: the fan-out itself runs in the
//! node's code plane (it owns the overlay streams), reached through the same
//! channel-of-requests seam every other mesh-dependent route uses — a daemon
//! whose embedder wired no code plane answers 503, never hangs.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::{NodeHandle, error_response, hex_bytes};

/// one peer's receipt for one staged artifact.
#[derive(Debug, Clone, Serialize)]
pub struct CodePeerReceipt {
    /// hex of the peer's public key.
    pub peer: String,
    /// "stored" | "already-have" | a refusal/failure reason.
    pub status: String,
    pub ok: bool,
}

/// a stage-to-network request: fan the (locally resident) digest out to
/// every member and report per-peer receipts.
pub struct CodeStageRequest {
    pub digest: [u8; 32],
    pub kind: u8,
    pub reply: tokio::sync::oneshot::Sender<Vec<CodePeerReceipt>>,
}

/// the code plane's request lane — `None` on daemons without a mesh.
pub type CodeStageLane = tokio::sync::mpsc::Sender<CodeStageRequest>;

/// the one artifact kind the lane admits today (see the node's code plane).
pub const CODE_KIND_MODULE: u8 = 1;

#[derive(Deserialize)]
pub(crate) struct StageParams {
    /// skip the network fan-out: ingest locally and report the digest only.
    #[serde(default = "default_fanout")]
    fanout: bool,
}

fn default_fanout() -> bool {
    true
}

#[derive(Serialize)]
struct StageReply {
    digest: String,
    len: u64,
    receipts: Vec<CodePeerReceipt>,
}

/// body cap for the stage lane, set EXPLICITLY rather than inherited.
///
/// Without a layer axum applies its implicit 2 MiB default, and the largest
/// module artifact in tree — `crates/modules/apps/runs/component.wasm`, 1.73 MB
/// — already sits at 82% of that. The next module to cross it would have been
/// un-stageable behind an opaque tower error carrying no reason token.
///
/// 16 MiB is ~9x that artifact: room for a component and its mapper, while
/// still BOUNDING an ingest
/// this route fans out to every member over the code plane. Disabling the limit
/// would be strictly worse than refusing — one operator-credentialed caller
/// could drive an unbounded local buffer and an unbounded network fan-out.
pub use node_work::MAX_STAGED_BLOB_BYTES as MAX_MODULE_ARTIFACT_BYTES;

/// a refusal carrying a stable snake_case `reason` beside the message — the
/// admin namespace's body shape (`admin::refuse`), so an operator's client
/// branches on the token instead of parsing tower's prose. `debug`, not `warn`:
/// only a credentialed operator reaches this route, but they can still loop.
fn refuse_stage(status: StatusCode, reason: &'static str, message: &'static str) -> Response {
    tracing::debug!(
        target: "ducktape::admin",
        reason,
        status = status.as_u16(),
        "module-code stage refused"
    );
    (
        status,
        Json(serde_json::json!({ "error": message, "reason": reason })),
    )
        .into_response()
}

/// POST /v1/admin/module-code/stage — body is the raw artifact bytes.
pub(crate) async fn stage_module_code(
    State(handle): State<NodeHandle>,
    Query(params): Query<StageParams>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Response {
    // the `DefaultBodyLimit` layer stops reading past the cap and the extractor
    // rejects — 413 for over-cap, anything else is a body that never arrived.
    // Both get a named reason; tower's own text says only "length limit
    // exceeded", which no client can branch on.
    let body = match body {
        Ok(bytes) => bytes,
        Err(rejection) => {
            let over_cap = rejection.status() == StatusCode::PAYLOAD_TOO_LARGE;
            return match over_cap {
                true => refuse_stage(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "module_artifact_too_large",
                    "module artifact exceeds this node's stage body cap",
                ),
                false => refuse_stage(
                    StatusCode::BAD_REQUEST,
                    "module_artifact_unreadable",
                    "could not read the module artifact body",
                ),
            };
        }
    };
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "empty artifact body");
    }
    if let Err(error) = module_artifact::ArtifactRef::decode(&body) {
        return error_response(StatusCode::BAD_REQUEST, &error);
    }
    let len = body.len() as u64;
    // ingest-by-value: the operator upload buffers once here, then lives on
    // disk (the store's large-blob path never parks it in memory). the WIRE
    // transfers to peers stream windowed — only this local hop buffers.
    let digest = handle.blobs.put_chunk(body.to_vec());
    let receipts = if params.fanout {
        let Some(lane) = handle.code_stage.clone() else {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "this daemon runs no code plane (no mesh) — staged locally only",
            );
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        let request = CodeStageRequest {
            digest,
            kind: CODE_KIND_MODULE,
            reply,
        };
        if lane.send(request).await.is_err() {
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "code plane not running");
        }
        match rx.await {
            Ok(receipts) => receipts,
            Err(_) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "code plane dropped the stage reply",
                );
            }
        }
    } else {
        Vec::new()
    };
    Json(StageReply {
        digest: hex_bytes(&digest),
        len,
        receipts,
    })
    .into_response()
}

#[derive(Serialize)]
struct ResidencyReply {
    digest: String,
    resident: bool,
    len: Option<u64>,
}

/// GET /v1/admin/module-code/{digest} — local residency of one artifact.
pub(crate) async fn module_code_status(
    State(handle): State<NodeHandle>,
    Path(digest): Path<String>,
) -> Response {
    let Some(raw) = duckfs_core::from_hex_32(&digest) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "digest must be 64 characters of lowercase hex",
        );
    };
    Json(ResidencyReply {
        digest,
        resident: handle.blobs.has_chunk(&raw),
        len: handle.blobs.chunk_len(&raw),
    })
    .into_response()
}
