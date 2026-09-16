//! Pages presence socket and account admission for bounded node proofs.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::signed_req::{refuse, verify_signed_request};
use crate::{NodeHandle, error_response, hex_bytes};

/// One editor's ephemeral caret/selection inside a page. `block_id=None`
/// means the peer is viewing the page without a body-block caret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor {
    pub block_id: Option<String>,
    pub anchor: u32,
    pub head: u32,
}

/// page-presence control from the webview to the overlay hub.
pub enum PresenceControlIn {
    Cursor(PageCursor),
}

/// page-presence control from a mesh peer to the webview.
pub enum PresenceControlOut {
    PeerCursor { peer: [u8; 32], cursor: PageCursor },
}

/// A lean Pages presence session. It owns an authenticated overlay control
/// flow and carries no media.
pub struct PresenceSession {
    pub recipients: tokio::sync::watch::Sender<Vec<[u8; 32]>>,
    pub control_in: tokio::sync::mpsc::Sender<PresenceControlIn>,
    pub control_out: tokio::sync::mpsc::Receiver<PresenceControlOut>,
}

pub struct PresenceSessionRequest {
    pub page_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<PresenceSession, String>>,
}

/// Request lane into the Pages presence overlay runtime.
pub type PresenceLane = tokio::sync::mpsc::Sender<PresenceSessionRequest>;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceClientControl {
    Recipients {
        peers: Vec<String>,
    },
    Cursor {
        block_id: Option<String>,
        anchor: u32,
        head: u32,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceServerControl {
    PeerCursor {
        peer: String,
        block_id: Option<String>,
        anchor: u32,
        head: u32,
    },
}

const MAX_REALTIME_ID_BYTES: usize = 256;
const MAX_PRESENCE_WS_MESSAGE_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
pub struct PresenceParams {
    page: String,
    #[serde(default)]
    token: Option<String>,
}

/// Why the realtime hub turned an upgrade away past the signature check.
/// status and the stable `reason` token derive from the variant, like
/// [`crate::signed_req::WriteRefusal`], so they cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuddleRefusal {
    /// a well-formed signature by a key the identity directory does not know.
    KeyWithoutAccount,
}

impl HuddleRefusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::KeyWithoutAccount => "key_without_account",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::KeyWithoutAccount => "the signing key holds no account on this network",
        }
    }
}

/// the refusal body: 403, the reason token, nothing about the URI. LATCHED at
/// `warn` like `signed_req::refuse`, and for the same reason — an upgrade is
/// client-driven and the ring must survive a loop of them.
fn refuse_huddle(refusal: HuddleRefusal) -> Response {
    static REFUSED: crate::log::Latch = crate::log::Latch::new(50);
    if let Some(occurrences) = REFUSED.hit(refusal.reason()) {
        tracing::warn!(
            target: "ducktape::node",
            reason = refusal.reason(),
            occurrences,
            "realtime upgrade refused"
        );
    }
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": refusal.message(),
            "reason": refusal.reason(),
        })),
    )
        .into_response()
}

/// the account `key` holds, or the refusal to answer the caller with. the ONE
/// membership read every huddle gate makes — the node-proof mint and both
/// realtime upgrades admit a person only as a member of this network.
pub(crate) async fn account_holder(handle: &NodeHandle, key: Vec<u8>) -> Result<u64, Response> {
    match crate::handle::account_of_key(handle, key).await {
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err(refuse_huddle(HuddleRefusal::KeyWithoutAccount)),
        Err(reason) => Err(error_response(StatusCode::SERVICE_UNAVAILABLE, &reason)),
    }
}

/// Admit the workspace holder or a signed network account.
async fn admit(
    handle: &NodeHandle,
    token: Option<&str>,
    headers: &HeaderMap,
    path_and_query: &str,
) -> Result<(), Response> {
    let holds_workspace = token.is_some_and(|token| handle.workspace_secret_matches(token));
    if holds_workspace {
        return Ok(());
    }
    let key = verify_signed_request(handle, &Method::GET, path_and_query, headers, b"")
        .map_err(|refusal| refuse(path_and_query, refusal))?;
    account_holder(handle, key).await.map(|_| ())
}

/// Admit Pages presence using the workspace secret or account signature.
pub(crate) async fn presence_ws(
    State(handle): State<NodeHandle>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(params): Query<PresenceParams>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if params.page.is_empty() || params.page.len() > MAX_REALTIME_ID_BYTES {
        return error_response(StatusCode::BAD_REQUEST, "page must be 1..256 bytes");
    }
    let path_and_query = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    if let Err(refused) = admit(&handle, params.token.as_deref(), &headers, path_and_query).await {
        return refused;
    }
    let Some(call) = handle.presence.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "presence is not available on this node (no mesh realtime hub)",
        );
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .max_message_size(MAX_PRESENCE_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_PRESENCE_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| presence_session(socket, call, params.page))
}

async fn presence_session(mut socket: WebSocket, call: PresenceLane, page_id: String) {
    let (reply, opened) = tokio::sync::oneshot::channel();
    let request = PresenceSessionRequest { page_id, reply };
    const NO_HUB: &str = "this node runs no mesh realtime hub, so Pages presence is unavailable";
    let session = match call.send(request).await {
        Ok(()) => match opened.await {
            Ok(Ok(session)) => session,
            Ok(Err(refusal)) => {
                let _ = socket.send(Message::Text(refusal.into())).await;
                return;
            }
            Err(_) => {
                let _ = socket.send(Message::Text(NO_HUB.into())).await;
                return;
            }
        },
        Err(_) => {
            let _ = socket.send(Message::Text(NO_HUB.into())).await;
            return;
        }
    };
    let PresenceSession {
        recipients,
        control_in,
        mut control_out,
    } = session;
    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<PresenceClientControl>(&text) {
                        Ok(PresenceClientControl::Recipients { peers }) => {
                            let keys = peers
                                .iter()
                                .filter_map(|hex| duckfs_core::from_hex_32(hex))
                                .collect();
                            let _ = recipients.send(keys);
                        }
                        Ok(PresenceClientControl::Cursor { block_id, anchor, head })
                            if block_id
                                .as_ref()
                                .is_none_or(|id| !id.is_empty() && id.len() <= 256) =>
                        {
                            let _ = control_in.try_send(PresenceControlIn::Cursor(PageCursor {
                                block_id,
                                anchor,
                                head,
                            }));
                        }
                        Ok(PresenceClientControl::Cursor { .. }) | Err(_) => {}
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            control = control_out.recv() => match control {
                Some(PresenceControlOut::PeerCursor { peer, cursor }) => {
                    let message = PresenceServerControl::PeerCursor {
                        peer: hex_bytes(&peer),
                        block_id: cursor.block_id,
                        anchor: cursor.anchor,
                        head: cursor.head,
                    };
                    let text = serde_json::to_string(&message).expect("serializable presence");
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SECRET: &str = "d3adb33fd3adb33fd3adb33fd3adb33f";

    /// a handle whose service link minted [`TEST_SECRET`] — the same
    /// workspace-secret shape the gated stream topics stand on, which is
    /// exactly what `presence_ws` reuses.
    fn handle_with_secret() -> NodeHandle {
        let (handle, _cmds, _hub) = NodeHandle::channel();
        handle.with_service_link(crate::service_link::ServiceLink::new(Some(
            TEST_SECRET.into(),
        )))
    }

    const PATH: &str = "/v1/presence/ws?page=page-1";

    /// the refusal an unsigned, tokenless (or wrong-tokened) upgrade gets.
    async fn refusal_of(handle: &NodeHandle, token: Option<&str>) -> (StatusCode, String) {
        let response = admit(handle, token, &HeaderMap::new(), PATH)
            .await
            .expect_err("not admitted");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (status, body["reason"].as_str().unwrap().to_string())
    }

    /// #1715: a caller presenting no token, or the wrong one, is not admitted
    /// — the same "wrong is as good as absent" rule the ws topic gate proves.
    /// with no signature either, the refusal is the signature gate's own — on
    /// this keyless test handle, its "nothing to salt with" refusal.
    #[tokio::test]
    async fn the_exact_workspace_secret_admits_and_anything_else_falls_to_the_signature() {
        let handle = handle_with_secret();
        let unsalted = (StatusCode::INTERNAL_SERVER_ERROR, "node_unidentified");
        let (status, reason) = refusal_of(&handle, None).await;
        assert_eq!((status, reason.as_str()), unsalted);
        let (status, reason) = refusal_of(&handle, Some("not-the-secret")).await;
        assert_eq!((status, reason.as_str()), unsalted);
        assert!(
            matches!(
                admit(&handle, Some(TEST_SECRET), &HeaderMap::new(), PATH).await,
                Ok(())
            ),
            "the exact workspace secret must admit"
        );
    }

    /// a node with no workspace (no terminal plane, no minted secret) admits
    /// no token — fails closed, never open, on the one huddle/presence upgrade
    /// path that used to check nothing at all.
    #[tokio::test]
    async fn a_node_with_no_workspace_secret_admits_no_token() {
        let (bare, _cmds, _hub) = NodeHandle::channel();
        assert!(
            admit(&bare, Some(TEST_SECRET), &HeaderMap::new(), PATH)
                .await
                .is_err()
        );
        assert!(admit(&bare, None, &HeaderMap::new(), PATH).await.is_err());
    }
}
