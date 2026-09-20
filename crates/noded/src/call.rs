//! The realtime websocket legs: the huddle call socket (`GET /v1/call/ws`)
//! into the node's media executor, Pages presence, and the account admission
//! both share with the bounded node-proof mint.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use commonware_cryptography::Signer as _;
use serde::{Deserialize, Serialize};

use crate::signed_req::{refuse, verify_signed_request};
use crate::{NodeHandle, chat, error_response, hex_bytes};

// ---- the call lane ----------------------------------------------------------
// the client end of a huddle: GET /v1/call/ws?channel=<id> upgrades to a socket
// that carries the huddle's audio, camera video, and call control together.
// the handler asks the node's media executor for a session over the request
// lane below; a daemon without one answers 503, and every refusal path says
// WHY as one text frame before closing.
//
// frames cross this leg OPAQUE: binary frames are `media_service::call_wire`
// bytes and text frames are json control, and the realtime guest the executor
// drives owns both vocabularies. the ONE text frame the node reads itself is
// `recipients` — the roster is what host-side admission gates on, so the host
// parses it and hands the guest the keys.

/// one frame on the call socket, as the guest sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// what the socket hands the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallClientIn {
    Frame(CallFrame),
    /// the client's `recipients` control frame, decoded: the raw node keys
    /// this session fans out to.
    Recipients(Vec<[u8; 32]>),
}

/// what the executor hands the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallServerOut {
    Frame(CallFrame),
    /// the session is over; `reason` is the last text frame the client sees.
    Close { reason: String },
}

/// one live huddle session's channel ends, executor ↔ websocket handler.
pub struct CallSession {
    pub to_hub: tokio::sync::mpsc::Sender<CallClientIn>,
    pub from_hub: tokio::sync::mpsc::Receiver<CallServerOut>,
}

/// a websocket handler's ask: open the call session for a channel. the
/// executor replies with the session's ends or a refusal string.
pub struct CallSessionRequest {
    pub channel_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<CallSession, String>>,
}

/// the request lane into the media executor.
pub type CallLane = tokio::sync::mpsc::Sender<CallSessionRequest>;

/// client → server control messages on the call socket (text frames). the
/// node decodes `recipients`; the other variants document the vocabulary the
/// guest reads.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallClientControl {
    /// replace the fan-out set with these hex node keys — the client tracks
    /// the consensus huddle roster; the hub drops its own key.
    Recipients { peers: Vec<String> },
    /// this client's ephemeral state; the hub beacons it to peers at 1 Hz.
    Beacon {
        muted: bool,
        camera_on: bool,
        #[serde(default)]
        sharing: bool,
        #[serde(default)]
        speaking: bool,
    },
    /// the decoder lost sync with `peer` — ask it for a keyframe.
    KeyframeRequest { peer: String },
}

/// server → client control messages on the call socket (text frames), as the
/// guest emits them.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallServerControl {
    /// a peer lost sync with US: encode the next frame as a keyframe.
    KeyframeRequest,
    /// a peer's 1 Hz beacon (ephemeral presence/state — never consensus).
    PeerBeacon {
        peer: String,
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
    /// send at no more than this (min across peers' loss reports).
    RateHint { max_kbps: u32 },
}

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

/// the ws frame/message ceiling on `/v1/call/ws` — this leg alone carries a
/// captured camera frame, and a keyframe runs several times the rate ladder's
/// per-frame budget; 1 MiB leaves that headroom while staying two orders of
/// magnitude under tungstenite's unbounded default.
const MAX_CALL_WS_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct CallParams {
    channel: String,
    /// this node's own 0600 workspace secret, presented as a query param
    /// because the upgrade itself — not a later ws frame — is the thing being
    /// admitted. the SAME secret the `/v1/ws` `Subscribe.token` presents.
    #[serde(default)]
    token: Option<String>,
}

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
    /// an account holder the channel's committed huddle roster does not name
    /// at THIS node.
    NotInHuddle,
}

impl HuddleRefusal {
    pub fn reason(self) -> &'static str {
        match self {
            Self::KeyWithoutAccount => "key_without_account",
            Self::NotInHuddle => "not_in_huddle",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::KeyWithoutAccount => "the signing key holds no account on this network",
            Self::NotInHuddle => {
                "the channel's huddle roster does not name this account at this node"
            }
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

/// what admitted a realtime upgrade. the ONE discriminant the call handler
/// branches on after the shared proof: a workspace holder is the device that
/// hosts this node and needs no roster read; an account holder is a device
/// pointed at this node from elsewhere, admitted by name.
#[derive(Debug)]
enum Admitted {
    /// the caller read this node's own 0600 workspace secret.
    Workspace,
    /// the caller signed the upgrade with a key holding this account.
    Account(u64),
}

/// Admit the workspace holder (`?token=`) or a network account (the
/// `signed_req` header trio over `GET` + this exact path+query + an empty
/// body, by a key that holds an account).
async fn admit(
    handle: &NodeHandle,
    token: Option<&str>,
    headers: &HeaderMap,
    path_and_query: &str,
) -> Result<Admitted, Response> {
    let holds_workspace = token.is_some_and(|token| handle.workspace_secret_matches(token));
    if holds_workspace {
        return Ok(Admitted::Workspace);
    }
    let key = verify_signed_request(handle, &Method::GET, path_and_query, headers, b"")
        .map_err(|refusal| refuse(path_and_query, refusal))?;
    account_holder(handle, key).await.map(Admitted::Account)
}

/// the chat channel as committed, read over the command lane.
async fn query_channel(
    handle: &NodeHandle,
    channel_id: &str,
) -> Result<Option<chat::Channel>, String> {
    let (reply, rx) = futures::channel::oneshot::channel();
    handle
        .send(crate::NodeCommand::Query {
            target: "chat".into(),
            req: chat::encode_query(&chat::ChatQuery::Channel {
                channel_id: channel_id.to_string(),
            }),
            reply,
        })
        .await
        .map_err(|_| "actor gone".to_string())?;
    let bytes = rx
        .await
        .map_err(|_| "reply dropped".to_string())?
        .map_err(|refused| refused.message)?;
    let chat::ChatReply::Channel(channel) = chat::decode_reply(&bytes)?;
    Ok(channel)
}

/// does the channel's committed huddle roster name `account`, routed through
/// THIS node? the socket carries that person's media onto the overlay under
/// this node's key, so the roster entry has to be the one this node's own
/// proof minted (`/v1/huddle/node-proof`) — a member joined elsewhere opens
/// their socket there.
async fn in_huddle_here(handle: &NodeHandle, channel_id: &str, account: u64) -> bool {
    let Ok(Some(channel)) = query_channel(handle, channel_id).await else {
        return false;
    };
    let this_node = handle
        .node_signer
        .as_ref()
        .map(|signer| signer.public_key().as_ref().to_vec());
    channel.huddle.iter().any(|member| {
        member.party == chat::Party::Account(account) && Some(&member.node) == this_node.as_ref()
    })
}

pub(crate) async fn call_ws(
    State(handle): State<NodeHandle>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(params): Query<CallParams>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if params.channel.is_empty() || params.channel.len() > MAX_REALTIME_ID_BYTES {
        return error_response(StatusCode::BAD_REQUEST, "channel must be 1..256 bytes");
    }
    let path_and_query = uri.path_and_query().map_or(uri.path(), |pq| pq.as_str());
    let admitted = match admit(&handle, params.token.as_deref(), &headers, path_and_query).await {
        Ok(admitted) => admitted,
        Err(refused) => return refused,
    };
    if let Admitted::Account(account) = admitted {
        let named_here = in_huddle_here(&handle, &params.channel, account).await;
        if !named_here {
            return refuse_huddle(HuddleRefusal::NotInHuddle);
        }
    }
    let Some(call) = handle.call.clone() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "calls are not available on this node (no mesh call hub)",
        );
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade
        .max_message_size(MAX_CALL_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_CALL_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| call_session(socket, call, params.channel))
}

/// the raw node keys a `recipients` frame names, or None for any other text.
fn recipients_of(text: &str) -> Option<Vec<[u8; 32]>> {
    let Ok(CallClientControl::Recipients { peers }) = serde_json::from_str(text) else {
        return None;
    };
    Some(
        peers
            .iter()
            .filter_map(|hex| duckfs_core::from_hex_32(hex))
            .collect(),
    )
}

/// pump one huddle's frames between the client websocket and the executor
/// session: client frames cross as-is (the guest owns their vocabulary),
/// except `recipients`, which the node decodes for its own admission. either
/// side closing ends the session — dropping `to_hub` is what the executor
/// watches.
async fn call_session(mut socket: WebSocket, call: CallLane, channel_id: String) {
    let (reply, opened) = tokio::sync::oneshot::channel();
    let request = CallSessionRequest { channel_id, reply };
    // every refusal path says WHY as a text frame before closing — the client
    // surfaces it as a session error instead of a silent no-op. Both
    // lane-closed cases in one sentence, because the handler cannot tell them
    // apart.
    const NO_HUB: &str = "this node runs no call hub, so it cannot host a huddle: it has no mesh \
                          overlay (wireguard_listen unset, or the fake effect — huddle media \
                          rides the overlay), is a sync-only observer, or its hub stopped.";
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
    let CallSession {
        to_hub,
        mut from_hub,
    } = session;
    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Binary(bytes))) => {
                    // full lane = the executor is behind; late media is dead
                    // media, so drop the frame rather than backpressure.
                    let _ = to_hub.try_send(CallClientIn::Frame(CallFrame::Binary(bytes.to_vec())));
                }
                Some(Ok(Message::Text(text))) => match recipients_of(&text) {
                    // the roster steers admission, so it is never shed.
                    Some(keys) => {
                        if to_hub.send(CallClientIn::Recipients(keys)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = to_hub.try_send(CallClientIn::Frame(CallFrame::Text(text.to_string())));
                    }
                },
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            outbound = from_hub.recv() => match outbound {
                Some(CallServerOut::Frame(CallFrame::Binary(bytes))) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Some(CallServerOut::Frame(CallFrame::Text(text))) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(CallServerOut::Close { reason }) => {
                    let _ = socket.send(Message::Text(reason.into())).await;
                    break;
                }
                None => break, // the executor ended the session without a word (shutting down).
            },
        }
    }
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
    // an account holder is admitted as such — a page has no committed roster
    // to check a name against, and presence carries cursors, not media.
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
        .max_message_size(usize::MAX)
        .max_frame_size(usize::MAX)
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

    /// the node reads exactly one client control frame — `recipients`, whose
    /// keys steer admission; a malformed key is skipped, any other frame (a
    /// beacon, an unknown type, garbage) is the guest's to read.
    #[test]
    fn only_recipients_is_decoded_host_side() {
        let key = [7u8; 32];
        let text = format!(
            r#"{{"type":"recipients","peers":["{}","not-hex"]}}"#,
            hex_bytes(&key)
        );
        assert_eq!(recipients_of(&text), Some(vec![key]));
        assert_eq!(
            recipients_of(r#"{"type":"beacon","muted":true,"camera_on":false}"#),
            None
        );
        assert_eq!(recipients_of("garbage"), None);
    }

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
                Ok(Admitted::Workspace)
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
