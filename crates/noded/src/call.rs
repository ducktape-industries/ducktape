//! the call lane: the webview end of a huddle (`GET /v1/call/ws`) and the
//! typed session/control types it shares with the node's call hub.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use commonware_cryptography::Signer as _;
use serde::{Deserialize, Serialize};

use crate::signed_req::{refuse, verify_signed_request};
use crate::{NodeHandle, error_response, hex_bytes};

// ---- the call lane ----------------------------------------------------------
// the webview end of a huddle: GET /v1/call/ws?channel=<id> upgrades to a
// typed socket that carries the huddle's audio, camera video, and call control
// together. the handler asks the node's call hub for a session over the request
// lane below; a daemon without a hub answers 503, and every refusal path says
// WHY as one text frame before closing.
//
// the binary frame layout (tag byte, header fields, BE headers/LE pcm payload
// per D1) and its encode/decode functions live in `media_service::call_wire` — the
// single definition site this handler ports onto below. text frames are json
// control: client→server `CallClientControl` (recipients / beacon /
// keyframe_request), server→client `CallServerControl` (keyframe_request /
// peer_beacon / rate_hint).
//
// the hub side lives with the mesh (validators and standing residents run one): it
// fragments/reassembles the video ends over `Service::Video` and routes control
// (keyframe kicks, presence beacons, rate hints — see `media_service::video`).

/// call-control the WEBVIEW asks the hub to act on (webview → hub).
pub enum CallControlIn {
    /// our local presence/state, pushed immediately AND repeated at 1 Hz as
    /// this session's beacon to every recipient. `sharing` = the video lane is a
    /// screen share rather than the camera. `speaking` = the mic's voice gate
    /// is open right now.
    Beacon {
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
    /// our decoder lost `peer`'s stream — ask `peer` for a fresh keyframe.
    KeyframeRequest { peer: [u8; 32] },
}

/// call-control the hub surfaces to the WEBVIEW (hub → webview).
pub enum CallControlOut {
    /// a peer's receiver asked us to send it a fresh keyframe — the webview
    /// tells its encoder to emit one (rate-limited to ≤1 Hz by the hub).
    KeyframeRequest,
    /// a peer's 1 Hz presence beacon — drives the tile's mute/camera badges +
    /// the screen-share treatment.
    PeerBeacon {
        peer: [u8; 32],
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
    /// the effective outbound bitrate cap (min of every peer's hint) — the
    /// webview retargets its encoder. emitted only when the value changes.
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

/// one live huddle session's channel ends, hub ↔ websocket handler / gateway.
pub struct CallSession {
    /// captured mic frames, exactly [`media_service::voice::FRAME_SAMPLES`] samples each.
    pub pcm_in: tokio::sync::mpsc::Sender<Vec<i16>>,
    /// mixed playout frames at the 20 ms tick, same shape.
    pub mixed_out: tokio::sync::mpsc::Receiver<Vec<i16>>,
    /// where this session's frames fan out: the huddle roster's node keys
    /// (raw ed25519 bytes), steered by the client as consensus state changes.
    pub recipients: tokio::sync::watch::Sender<Vec<[u8; 32]>>,
    /// captured camera frames webview → hub (fragmented onto `Service::Video`).
    pub video_in: tokio::sync::mpsc::Sender<media_service::call_wire::CapturedFrame>,
    /// reassembled peer camera frames hub → webview.
    pub video_out: tokio::sync::mpsc::Receiver<media_service::call_wire::PeerFrame>,
    /// call-control webview → hub (local beacon, keyframe asks).
    pub control_in: tokio::sync::mpsc::Sender<CallControlIn>,
    /// call-control hub → webview (peer beacons, keyframe kicks, rate hints).
    pub control_out: tokio::sync::mpsc::Receiver<CallControlOut>,
}

/// a websocket handler's ask: open (or replace) the call session for a
/// channel. the hub replies with the session's ends or a refusal string.
pub struct CallSessionRequest {
    pub channel_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<CallSession, String>>,
}

/// A lean Pages presence session. It shares the authenticated overlay control
/// plane with huddles but owns a separate flow and carries no media.
pub struct PresenceSession {
    pub recipients: tokio::sync::watch::Sender<Vec<[u8; 32]>>,
    pub control_in: tokio::sync::mpsc::Sender<PresenceControlIn>,
    pub control_out: tokio::sync::mpsc::Receiver<PresenceControlOut>,
}

pub struct PresenceSessionRequest {
    pub page_id: String,
    pub reply: tokio::sync::oneshot::Sender<Result<PresenceSession, String>>,
}

/// Both live, off-consensus session types share one request lane into the
/// overlay hub. The hub keeps one huddle and one Pages session concurrently.
pub enum RealtimeSessionRequest {
    Call(CallSessionRequest),
    Presence(PresenceSessionRequest),
}

/// the request lane into the realtime overlay hub.
pub type CallLane = tokio::sync::mpsc::Sender<RealtimeSessionRequest>;

/// client → server control messages on the call socket (text frames).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CallClientControl {
    /// replace the fan-out set with these hex node keys (self excluded —
    /// the client tracks the consensus huddle roster).
    Recipients { peers: Vec<String> },
    /// this client's ephemeral state; the hub beacons it to peers at 1 Hz.
    /// `sharing` defaults false so a pre-share client (no field) still parses.
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

/// server → client control messages on the call socket (text frames).
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
    RateHint {
        max_kbps: u32,
    },
}

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

/// the longest channel/page id this node will accept on a realtime ws upgrade
/// — mirrors the bound `presence_ws` already put on `page`, now shared so
/// `call_ws` carries the same ceiling.
const MAX_REALTIME_ID_BYTES: usize = 256;

/// the ws frame/message ceiling on `/v1/call/ws` and `/v1/presence/ws` —
/// larger than [`crate::stream::MAX_WS_MESSAGE_BYTES`] because this leg alone
/// carries a captured VP8 camera frame (`CallClientControl`/binary video):
/// `RATE_LADDER_KBPS`'s top rung (1200 kbps, `media_service::video::control`)
/// budgets ~5 KiB/frame at 30 fps, and a keyframe runs several times that —
/// 1 MiB leaves that headroom with more than 10x to spare while staying two
/// orders of magnitude under tungstenite's unbounded 64 MiB default.
const MAX_CALL_WS_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct CallParams {
    channel: String,
    /// this node's own 0600 workspace secret ([`crate::services::LINK_TOKEN_FILE`]),
    /// presented as a query param because the upgrade itself — not a later ws
    /// frame — is the thing being admitted. The SAME secret the `/v1/ws`
    /// `Subscribe.token` and `ServiceAttach` already ask for; see
    /// [`crate::stream::Admission::Workspace`].
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
    match crate::term_consensus::account_of_key(handle, key).await {
        Ok(Some(account)) => Ok(account),
        Ok(None) => Err(refuse_huddle(HuddleRefusal::KeyWithoutAccount)),
        Err(reason) => Err(error_response(StatusCode::SERVICE_UNAVAILABLE, &reason)),
    }
}

/// what admitted a realtime upgrade. the ONE discriminant the two handlers
/// branch on after the shared proof: a workspace holder is the device that
/// hosts this node and needs no roster read; an account holder is a device
/// pointed at this node from elsewhere, admitted by name.
enum Admitted {
    /// the caller read this node's own 0600 workspace secret.
    Workspace,
    /// the caller signed the upgrade with a key holding this account.
    Account(u64),
}

/// The realtime hub hands out live mic/camera/cursor bytes to whoever opens
/// the socket — the same class of bytes `run-output:`/`term:` gate behind
/// [`crate::stream::Admission::Workspace`]. So an upgrade is admitted by ONE of
/// two proofs, decided here before the upgrade is attempted (an unadmitted
/// caller learns nothing past its refusal, not even whether a hub exists):
///
/// - this node's workspace secret as `?token=` — the device that hosts the
///   node, which is what the `/v1/ws` `Subscribe.token` already presents; or
/// - the data-plane signature trio (`signed_req`) over `GET` + this exact
///   path+query + an empty body, by a key that holds an account — a device
///   pointed at a node it does not host. the caller's OWN proof of possession,
///   so it stays a header trio and never rides the query string.
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

/// does the channel's committed huddle roster name `account`, routed through
/// THIS node? the socket carries that person's media onto the overlay under
/// this node's key, so the roster entry has to be the one this node's own
/// proof minted (`/v1/huddle/node-proof`) — a member joined elsewhere opens
/// their socket there.
async fn in_huddle_here(handle: &NodeHandle, channel_id: &str, account: u64) -> bool {
    let Ok(Some(channel)) = crate::term_consensus::query_channel(handle, channel_id).await else {
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

/// pump one huddle's audio, camera video, and call control between the webview
/// websocket and the hub session. binary client frames are decoded via
/// `media_service::call_wire` and tag-dispatched: audio → `pcm_in`, captured video →
/// `video_in`; the hub's `mixed_out`/`video_out`/`control_out` ends flow back
/// as `media_service::call_wire`-encoded binary + json text. text client frames steer
/// fan-out and carry beacons/keyframe asks. either side closing ends the
/// session — dropping the ends is the teardown signal the hub watches.
async fn call_session(mut socket: WebSocket, call: CallLane, channel_id: String) {
    let (reply, opened) = tokio::sync::oneshot::channel();
    let request = RealtimeSessionRequest::Call(CallSessionRequest { channel_id, reply });
    // every refusal path says WHY as a text frame before closing — the client
    // surfaces it as a session error instead of a silent no-op.
    // Both lane-closed cases in one sentence, because the handler cannot tell
    // them apart — and "no live call hub" alone left the user with a red
    // "Voice connection failed." and nothing to act on.
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
                // hub dropped the reply — shutting down.
                let _ = socket.send(Message::Text(NO_HUB.into())).await;
                return;
            }
        },
        Err(_) => {
            // the request lane is closed: a mode that never runs a hub
            // (parked joiner, sync-only observer) or a dead hub thread.
            let _ = socket.send(Message::Text(NO_HUB.into())).await;
            return;
        }
    };
    let CallSession {
        pcm_in,
        mut mixed_out,
        recipients,
        video_in,
        mut video_out,
        control_in,
        mut control_out,
    } = session;
    loop {
        tokio::select! {
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Binary(bytes))) => {
                    if let Some(frame) = media_service::call_wire::decode_audio(&bytes) {
                        // full lane = the hub is behind; late audio is dead
                        // audio, so drop the frame rather than backpressure.
                        let _ = pcm_in.try_send(frame);
                    } else if let Some(frame) = media_service::call_wire::decode_captured(&bytes) {
                        let _ = video_in.try_send(frame);
                    } // unknown/short frame — drop, stay alive
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<CallClientControl>(&text) {
                        Ok(CallClientControl::Recipients { peers }) => {
                            let keys: Vec<[u8; 32]> = peers
                                .iter()
                                .filter_map(|hex| duckfs_core::from_hex_32(hex))
                                .collect();
                            let _ = recipients.send(keys);
                        }
                        Ok(CallClientControl::Beacon {
                            muted,
                            camera_on,
                            sharing,
                            speaking,
                        }) => {
                            let _ = control_in.try_send(CallControlIn::Beacon {
                                muted,
                                camera_on,
                                sharing,
                                speaking,
                            });
                        }
                        Ok(CallClientControl::KeyframeRequest { peer }) => {
                            if let Some(key) = duckfs_core::from_hex_32(&peer) {
                                let _ = control_in
                                    .try_send(CallControlIn::KeyframeRequest { peer: key });
                            }
                        }
                        Err(_) => {} // unknown control — ignore, stay alive
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            mixed = mixed_out.recv() => match mixed {
                Some(frame) => {
                    let bytes = media_service::call_wire::encode_audio(&frame);
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break, // the hub ended the session (replaced by a newer join).
            },
            video = video_out.recv() => match video {
                Some(frame) => {
                    let bytes = media_service::call_wire::encode_peer(&frame);
                    if socket.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            control = control_out.recv() => match control {
                Some(out) => {
                    let message = match out {
                        CallControlOut::KeyframeRequest => CallServerControl::KeyframeRequest,
                        CallControlOut::PeerBeacon { peer, muted, camera_on, sharing, speaking } => {
                            CallServerControl::PeerBeacon {
                                peer: hex_bytes(&peer),
                                muted,
                                camera_on,
                                sharing,
                                speaking,
                            }
                        }
                        CallControlOut::RateHint { max_kbps } => {
                            CallServerControl::RateHint { max_kbps }
                        }
                    };
                    let text = serde_json::to_string(&message).expect("serializable control");
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
}

/// Pages presence: the same two proofs as [`call_ws`]. an account holder is
/// admitted as such — a page has no committed roster to check a name against,
/// and presence carries cursors, not media.
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
    let Some(call) = handle.call.clone() else {
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
        .max_message_size(MAX_CALL_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_CALL_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| presence_session(socket, call, params.page))
}

async fn presence_session(mut socket: WebSocket, call: CallLane, page_id: String) {
    let (reply, opened) = tokio::sync::oneshot::channel();
    let request = RealtimeSessionRequest::Presence(PresenceSessionRequest { page_id, reply });
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

    /// a handle whose terminal plane minted [`TEST_SECRET`] — the same
    /// workspace-secret shape [`crate::stream::Admission::Workspace`] gates on,
    /// which is exactly what `call_ws`/`presence_ws` now reuse.
    fn handle_with_secret() -> NodeHandle {
        let (handle, _cmds, _hub) = NodeHandle::channel();
        handle.with_terminals(crate::term::TerminalSessions::new(
            crate::term::TermRing::default(),
            crate::term::TermCommandRing::default(),
            Some(TEST_SECRET.into()),
        ))
    }

    const PATH: &str = "/v1/call/ws?channel=general";

    /// the refusal an unsigned, tokenless (or wrong-tokened) upgrade gets.
    async fn refusal_of(handle: &NodeHandle, token: Option<&str>) -> (StatusCode, String) {
        let response = admit(handle, token, &HeaderMap::new(), PATH)
            .await
            .err()
            .expect("not admitted");
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
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
        assert!(admit(&bare, Some(TEST_SECRET), &HeaderMap::new(), PATH).await.is_err());
        assert!(admit(&bare, None, &HeaderMap::new(), PATH).await.is_err());
    }
}
