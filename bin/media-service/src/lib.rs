//! Socket-activated huddle service. Gateway authenticates callers; this
//! process admits only their committed room membership and owns call frames.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use tokio::sync::{broadcast, mpsc};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub node_url: String,
    pub account: u64,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Caller {
    account: u64,
    node: [u8; 32],
}

fn caller(headers: &HeaderMap, token: &[u8; 64], config: &Config) -> Option<Caller> {
    let field = |name| {
        let mut values = headers.get_all(name).iter();
        let value = values.next()?.to_str().ok()?;
        values.next().is_none().then_some(value)
    };
    let presented = field("x-duck-upstream-token")?;
    let authenticated = bool::from(presented.as_bytes().ct_eq(token));
    if !authenticated {
        return None;
    }
    let route_account: u64 = field("x-duck-route-account")?.parse().ok()?;
    let revision: u64 = field("x-duck-route-revision")?.parse().ok()?;
    let installed_route = route_account == config.account
        && field("x-duck-route-label")? == config.label
        && revision > 0;
    if !installed_route {
        return None;
    }
    let account = field("x-duck-caller-account")?.parse().ok()?;
    let node_text = field("x-duck-caller-node")?;
    let mut node = [0; 32];
    let canonical_key = node_text.len() == 64
        && node_text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !canonical_key {
        return None;
    }
    for (out, digits) in node.iter_mut().zip(node_text.as_bytes().chunks_exact(2)) {
        *out = u8::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()?;
    }
    Some(Caller { account, node })
}

type Roster = BTreeSet<Caller>;

struct Membership {
    roster: Roster,
    changes: BoxStream<'static, Result<(), String>>,
}

trait Authority: Send + Sync {
    fn refresh(&self, room: String) -> BoxFuture<'static, Result<Roster, String>>;
    fn open(&self, room: String) -> BoxFuture<'static, Result<Membership, String>>;
}

struct NodeAuthority {
    client: ducktape_rpc::Client,
    account: u64,
}

async fn roster(client: &ducktape_rpc::Client, room: &str, account: u64) -> Result<Roster, String> {
    let reply: chat::ChatReply = client
        .query(
            chat::DEFAULT_CHAT_TARGET,
            &chat::ChatQuery::Channel {
                channel_id: room.into(),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    let chat::ChatReply::Channel(Some(channel)) = reply else {
        return Err("room unavailable".into());
    };
    channel_roster(channel, account)
}

fn channel_roster(channel: chat::Channel, account: u64) -> Result<Roster, String> {
    if channel.owner != chat::Party::Account(account) {
        return Err("room belongs to another route owner".into());
    }
    channel
        .huddle
        .into_iter()
        .filter_map(|member| {
            let chat::Party::Account(account) = member.party else {
                return None;
            };
            Some(
                member
                    .node
                    .try_into()
                    .map(|node| Caller { account, node })
                    .map_err(|_| "invalid huddle node".to_owned()),
            )
        })
        .collect()
}

const FEED_LOST: &str = "room event feed lost";

/// The channel one committed chat write acts on, or `None` when the write
/// names none this service could be seated in.
///
/// Exhaustive on purpose: a new chat write must fail this build until someone
/// states which room it touches, because a write nobody routed is a write
/// this service would stop re-reading its roster for.
fn acted_channel(message: &chat::ChatMsg) -> Option<&str> {
    match message {
        chat::ChatMsg::CreateChannel { channel_id, .. }
        | chat::ChatMsg::CreateVoiceChannel { channel_id, .. }
        | chat::ChatMsg::RenameChannel { channel_id, .. }
        | chat::ChatMsg::SetChannelArchived { channel_id, .. }
        | chat::ChatMsg::PostMessage { channel_id, .. }
        | chat::ChatMsg::EditMessage { channel_id, .. }
        | chat::ChatMsg::DeleteMessage { channel_id, .. }
        | chat::ChatMsg::AddReaction { channel_id, .. }
        | chat::ChatMsg::RemoveReaction { channel_id, .. }
        | chat::ChatMsg::RegisterHook { channel_id, .. }
        | chat::ChatMsg::UnregisterHook { channel_id, .. }
        | chat::ChatMsg::SetMembership { channel_id, .. }
        | chat::ChatMsg::JoinHuddle { channel_id, .. }
        | chat::ChatMsg::LeaveHuddle { channel_id }
        | chat::ChatMsg::SweepHuddle { channel_id, .. } => Some(channel_id),
        // The module derives a DM's id from the pair that opens it, so the
        // payload carries no channel to compare against.
        chat::ChatMsg::CreateDmChannel { .. } => None,
    }
}

/// Whether one committed chat op can have changed who is in `room`.
///
/// A channel's huddle is written only by an op naming that channel, so an op
/// naming another one cannot have moved this roster. An op this service
/// cannot attribute to a channel — a payload that is not JSON, one that does
/// not decode — is treated as this room's: an unattributed change may be the
/// one that revoked this seat.
fn changes_room(op: &ducktape_rpc::StreamOp, room: &str) -> bool {
    let Some(payload) = op.payload.as_ref() else {
        return true;
    };
    let Ok(message) = chat::ChatMsg::deserialize(payload) else {
        return true;
    };
    acted_channel(&message).is_none_or(|channel| channel == room)
}

/// What one chat-module event means to a seat in `room`: `None` to ignore it,
/// `Some(Ok(()))` to read the canonical roster again before anything more is
/// forwarded, `Some(Err(..))` for a feed that will not deliver.
///
/// Chat is ONE module and a seat follows all of it, so every committed
/// message in every channel of the workspace arrives here. This narrows WHICH
/// events reach the roster gate and never what the gate does when one does: a
/// replay gap loses the ops themselves, so it re-reads like a change to this
/// room.
fn room_change(event: &ducktape_rpc::ModuleEvent, room: &str) -> Option<Result<(), String>> {
    match event {
        ducktape_rpc::ModuleEvent::Changed { op, .. } => changes_room(op, room).then_some(Ok(())),
        ducktape_rpc::ModuleEvent::Lagged { .. } => Some(Ok(())),
        ducktape_rpc::ModuleEvent::Ready { .. } | ducktape_rpc::ModuleEvent::Tip { .. } => None,
        ducktape_rpc::ModuleEvent::Refused { .. } => Some(Err(FEED_LOST.into())),
    }
}

impl Authority for NodeAuthority {
    fn refresh(&self, room: String) -> BoxFuture<'static, Result<Roster, String>> {
        let client = self.client.clone();
        let account = self.account;
        Box::pin(async move { roster(&client, &room, account).await })
    }

    fn open(&self, room: String) -> BoxFuture<'static, Result<Membership, String>> {
        let client = self.client.clone();
        let account = self.account;
        Box::pin(async move {
            let mut events = client
                .module_events(vec![chat::DEFAULT_CHAT_TARGET.into()], BTreeMap::new())
                .await
                .map_err(|error| error.to_string())?;
            // Subscribe before the snapshot: a leave between the two remains
            // queued and is consumed before the next media frame.
            match events.next().await {
                Some(Ok(ducktape_rpc::ModuleEvent::Ready { .. })) => {}
                _ => return Err("room event feed unavailable".into()),
            }
            let initial = roster(&client, &room, account).await?;
            let changes = events
                .filter_map(move |event| {
                    let room = room.clone();
                    async move {
                        match event {
                            Ok(event) => room_change(&event, &room),
                            Err(_) => Some(Err(FEED_LOST.into())),
                        }
                    }
                })
                .boxed();
            Ok(Membership {
                roster: initial,
                changes,
            })
        })
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Beacon {
    muted: bool,
    camera_on: bool,
    sharing: bool,
    speaking: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Beacon {
        muted: bool,
        camera_on: bool,
        sharing: bool,
        speaking: bool,
    },
}

/// Rather more than one second of the profile that measured this queue — 50
/// audio frames and 50 picture frames a second — and the ceiling on how stale
/// the head of a resumed seat's queue can be.
///
/// A peer further behind than this has already lost the call: the deployed
/// guest buffers three frames and discards the rest of any burst, so a minute
/// of backlog costs the service a minute of holding and shipping frames the
/// receiver throws away. Late real-time data is dead data — the queue keeps
/// the NEWEST second or so and drops the oldest media to make room.
///
/// A POWER OF TWO, because the ring rounds up to one and the ceiling should
/// be the number written here rather than the number that got rounded to.
/// `a_stalled_reader_resumes_on_the_newest_frames_and_the_drop_is_counted`
/// fails on a value that is not.
const QUEUE_FRAMES: usize = 128;

struct Participant {
    id: u64,
    caller: Caller,
    /// The roster, a peer's beacon, a peer leaving: the session state a late
    /// reader needs in order to make sense of the media it does get, so it is
    /// never what gets dropped to make room.
    control: mpsc::UnboundedSender<Message>,
    /// Audio and picture frames, newest [`QUEUE_FRAMES`] only.
    media: broadcast::Sender<Message>,
    beacon: Beacon,
}

impl Participant {
    /// Queue one outbound frame on the lane its kind belongs to. `false` once
    /// this seat's socket is gone, which is how the hub prunes it.
    fn queue(&self, message: Message) -> bool {
        match message {
            Message::Binary(_) => self.media.send(message).is_ok(),
            Message::Text(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_) => {
                self.control.send(message).is_ok()
            }
        }
    }
}

/// A seat's two outbound lanes, as the writer reads them.
struct Outbound {
    control: mpsc::UnboundedReceiver<Message>,
    media: broadcast::Receiver<Message>,
}

#[derive(Default)]
struct Hub {
    next: u64,
    rooms: BTreeMap<String, BTreeMap<u64, Participant>>,
}

fn peer(node: &[u8; 32]) -> String {
    node.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn beacon_message(caller: &Caller, beacon: &Beacon) -> Message {
    Message::Text(
        serde_json::json!({"type": "peer_beacon", "account": caller.account,
        "peer": peer(&caller.node), "muted": beacon.muted, "camera_on": beacon.camera_on,
        "sharing": beacon.sharing, "speaking": beacon.speaking})
        .to_string()
        .into(),
    )
}

impl Hub {
    fn join(
        &mut self,
        room: &str,
        caller: Caller,
        roster: &Roster,
    ) -> Result<(u64, Outbound), String> {
        let participants = self.rooms.entry(room.into()).or_default();
        let occupied = participants.contains_key(&caller.account)
            || participants
                .values()
                .any(|participant| participant.caller.node == caller.node);
        if occupied {
            return Err("room seat unavailable".into());
        }
        // Admission sends the current roster before subsequent media.
        let peers: Vec<_> = participants
            .values()
            .filter(|participant| roster.contains(&participant.caller))
            .map(|participant| {
                serde_json::json!({"account": participant.caller.account,
                "peer": peer(&participant.caller.node), "state": participant.beacon})
            })
            .collect();
        let (control, control_input) = mpsc::unbounded_channel();
        let (media, media_input) = broadcast::channel(QUEUE_FRAMES);
        let _ = control.send(Message::Text(
            serde_json::json!({"type": "ready", "peers": peers})
                .to_string()
                .into(),
        ));
        self.next += 1;
        let id = self.next;
        participants.insert(
            caller.account,
            Participant {
                id,
                caller,
                control,
                media,
                beacon: Beacon::default(),
            },
        );
        Ok((
            id,
            Outbound {
                control: control_input,
                media: media_input,
            },
        ))
    }

    fn leave(&mut self, room: &str, caller: &Caller, id: u64) {
        let Some(participants) = self.rooms.get_mut(room) else {
            return;
        };
        let owns_seat = participants
            .get(&caller.account)
            .is_some_and(|seat| seat.id == id);
        if !owns_seat {
            return;
        }
        participants.remove(&caller.account);
        let left = Message::Text(
            serde_json::json!({"type": "peer_left", "account": caller.account,
            "peer": peer(&caller.node)})
            .to_string()
            .into(),
        );
        participants.retain(|_, participant| participant.queue(left.clone()));
        if participants.is_empty() {
            self.rooms.remove(room);
        }
    }

    fn relay(
        &mut self,
        room: &str,
        caller: &Caller,
        id: u64,
        roster: &Roster,
        message: Message,
    ) -> Result<(), String> {
        let participants = self.rooms.get_mut(room).ok_or("room ended")?;
        let source = participants
            .get_mut(&caller.account)
            .ok_or("session ended")?;
        if source.id != id {
            return Err("session superseded".into());
        }
        let message = match message {
            Message::Text(text) => {
                let control: Control =
                    serde_json::from_str(&text).map_err(|_| "invalid call control")?;
                match control {
                    Control::Beacon {
                        muted,
                        camera_on,
                        sharing,
                        speaking,
                    } => {
                        if camera_on && sharing {
                            return Err("ambiguous capture source".into());
                        }
                        source.beacon = Beacon {
                            muted,
                            camera_on,
                            sharing,
                            speaking: speaking && !muted,
                        };
                    }
                }
                beacon_message(caller, &source.beacon)
            }
            Message::Binary(bytes) => {
                match bytes.first().copied() {
                    Some(1) => {
                        let pcm = media_service::call_wire::decode_audio(&bytes)
                            .ok_or("invalid audio frame")?;
                        if source.beacon.muted {
                            return Ok(());
                        }
                        // Audio is tagged with authenticated source identity;
                        // each guest owns its jitter buffer and playout mix.
                        let mut outgoing = Vec::with_capacity(41 + pcm.len() * 2);
                        outgoing.push(4);
                        outgoing.extend_from_slice(&caller.account.to_be_bytes());
                        outgoing.extend_from_slice(&caller.node);
                        outgoing.extend_from_slice(&bytes[1..]);
                        Message::Binary(outgoing.into())
                    }
                    Some(2) => {
                        let frame = media_service::call_wire::decode_captured(&bytes)
                            .ok_or("invalid picture frame")?;
                        let capturing = source.beacon.camera_on || source.beacon.sharing;
                        if !capturing {
                            return Ok(());
                        }
                        Message::Binary(
                            media_service::call_wire::encode_peer(
                                &media_service::call_wire::PeerFrame {
                                    peer: caller.node,
                                    keyframe: frame.keyframe,
                                    ts_ms: frame.ts_ms,
                                    data: frame.data,
                                },
                            )
                            .into(),
                        )
                    }
                    _ => return Err("invalid media tag".into()),
                }
            }
            Message::Ping(_) | Message::Pong(_) => return Ok(()),
            Message::Close(_) => return Err("session closed".into()),
        };
        participants.retain(|account, participant| {
            let recipient = *account != caller.account && roster.contains(&participant.caller);
            !recipient || participant.queue(message.clone())
        });
        Ok(())
    }
}

struct Service {
    config: Config,
    token: [u8; 64],
    authority: Arc<dyn Authority>,
    hub: Mutex<Hub>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoomQuery {
    channel: String,
}

pub fn router(config: Config, token: [u8; 64]) -> Result<axum::Router, String> {
    let client = ducktape_rpc::Client::new(&config.node_url).map_err(|error| error.to_string())?;
    let authority = Arc::new(NodeAuthority {
        client,
        account: config.account,
    });
    Ok(service_router(Arc::new(Service {
        config,
        token,
        authority,
        hub: Mutex::default(),
    })))
}

/// Every socket this service accepts carries paced real-time call frames, and
/// each frame is already a whole application message. Nagle's algorithm has
/// nothing to coalesce here, but it still holds a frame until the previous
/// one is acknowledged — which on a long link adds a whole round trip to
/// every frame and delivers them in clumps a receiver's jitter buffer cannot
/// absorb. The listener is where this belongs: it is a property of the
/// transport, not of the call protocol riding it.
pub fn realtime_listener(
    listener: tokio::net::TcpListener,
) -> impl axum::serve::Listener<Addr = std::net::SocketAddr, Io = tokio::net::TcpStream> {
    use axum::serve::ListenerExt as _;
    listener.tap_io(|socket| {
        if let Err(error) = socket.set_nodelay(true) {
            tracing::warn!(
                target: "ducktape::call",
                reason = "nodelay_refused",
                %error,
                "accepted socket kept Nagle batching"
            );
        }
    })
}

fn service_router(service: Arc<Service>) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(upgrade))
        .with_state(service)
}

async fn upgrade(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Query(query): Query<RoomQuery>,
    socket: WebSocketUpgrade,
) -> Response {
    let Some(caller) = caller(&headers, &service.token, &service.config) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let valid_room = !query.channel.is_empty() && query.channel.len() <= 256;
    if !valid_room {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let membership = match service.authority.open(query.channel.clone()).await {
        Ok(membership) => membership,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if !membership.roster.contains(&caller) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (id, incoming) = match service.hub.lock().expect("media hub").join(
        &query.channel,
        caller.clone(),
        &membership.roster,
    ) {
        Ok(incoming) => incoming,
        Err(_) => return StatusCode::CONFLICT.into_response(),
    };
    let seat = Seat {
        id,
        service,
        room: query.channel,
        caller,
    };
    socket
        .max_frame_size(usize::MAX)
        .max_message_size(usize::MAX)
        .on_upgrade(move |socket| serve(socket, seat, membership, incoming))
}

struct Seat {
    id: u64,
    service: Arc<Service>,
    room: String,
    caller: Caller,
}

impl Drop for Seat {
    fn drop(&mut self) {
        self.service
            .hub
            .lock()
            .expect("media hub")
            .leave(&self.room, &self.caller, self.id);
    }
}

/// A stalled seat losing the oldest frames it had not read yet. Real-time
/// data this late is dead — the deployed guest keeps three frames of any
/// burst — but a seat that keeps losing them is a seat in trouble, so the
/// first loss is a warning and the rest are the same news with a bigger
/// number. This runs on the frame path: an unconditional `warn!` here would
/// evict the log ring it is reported into.
fn report_backlog_dropped(skipped: u64, dropped: u64) {
    let first_loss_for_this_seat = skipped == dropped;
    match first_loss_for_this_seat {
        true => tracing::warn!(
            target: "ducktape::call",
            reason = "peer_backlog_dropped",
            skipped,
            dropped,
            "a seat fell behind and its oldest queued frames were dropped"
        ),
        false => tracing::debug!(
            target: "ducktape::call",
            reason = "peer_backlog_dropped",
            skipped,
            dropped,
            "a seat fell behind and its oldest queued frames were dropped"
        ),
    }
}

/// The next frame to write, control lane first: the roster, a beacon and a
/// peer leaving are the state that explains the media, and a reader that fell
/// behind needs them before the frames they describe. `None` ends the seat.
async fn next_outgoing(output: &mut Outbound, dropped: &mut u64) -> Option<Message> {
    loop {
        let media = tokio::select! {
            biased;
            control = output.control.recv() => return control,
            media = output.media.recv() => media,
        };
        match media {
            Ok(message) => return Some(message),
            Err(broadcast::error::RecvError::Closed) => return None,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                *dropped += skipped;
                report_backlog_dropped(skipped, *dropped);
            }
        }
    }
}

async fn serve(socket: WebSocket, seat: Seat, mut membership: Membership, mut output: Outbound) {
    let (mut sink, mut incoming) = socket.split();
    let writer = async {
        let mut dropped = 0;
        while let Some(outgoing) = next_outgoing(&mut output, &mut dropped).await {
            if sink.send(outgoing).await.is_err() {
                break;
            }
        }
    };
    tokio::pin!(writer);
    loop {
        tokio::select! {
            biased;
            changed = membership.changes.next() => {
                let Some(Ok(())) = changed else { break; };
                // Pause all media while checking the committed change. An
                // asynchronous query inside a selected stream would keep
                // forwarding under the old roster until its answer arrived.
                let Ok(roster) = seat.service.authority.refresh(seat.room.clone()).await else { break; };
                if !roster.contains(&seat.caller) { break; }
                membership.roster = roster;
            }
            _ = &mut writer => break,
            incoming = incoming.next() => {
                let Some(Ok(incoming)) = incoming else { break; };
                let relayed = seat.service.hub.lock().expect("media hub").relay(&seat.room, &seat.caller, seat.id, &membership.roster, incoming);
                if relayed.is_err() { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_tungstenite::tungstenite::{
        Message as ClientMessage, client::IntoClientRequest as _,
    };

    /// The socket option `realtime_listener` exists to set, read back off a
    /// real accepted socket. `tap_io` swallows a failing option silently
    /// enough that only the accepted socket can say whether it took, and
    /// "verified by inspection" is not verification.
    #[tokio::test]
    async fn an_accepted_call_socket_has_nagle_disabled() {
        use axum::serve::Listener as _;

        let bound = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = bound.local_addr().unwrap();
        let mut listener = realtime_listener(bound);
        let dial = tokio::spawn(async move { tokio::net::TcpStream::connect(address).await });

        let (accepted, _) = listener.accept().await;
        let client = dial.await.unwrap().expect("client connects");

        assert!(
            accepted.nodelay().expect("read the accepted socket's option"),
            "an accepted call socket must not batch frames behind Nagle"
        );
        drop(client);
    }

    fn config() -> Config {
        Config {
            node_url: "http://127.0.0.1:9000".into(),
            account: 7,
            label: "media".into(),
        }
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-duck-upstream-token", "a".repeat(64)),
            ("x-duck-caller-account", "42".into()),
            ("x-duck-caller-node", "01".repeat(32)),
            ("x-duck-route-account", "7".into()),
            ("x-duck-route-label", "media".into()),
            ("x-duck-route-revision", "3".into()),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn canonical_room_owner_and_exact_member_node_are_required() {
        let mut channel = chat::Channel {
            id: "room".into(),
            name: "Room".into(),
            created_at: 0,
            head_seq: 0,
            post_policy: chat::PostPolicy::Open,
            hooks: Vec::new(),
            pinned: Vec::new(),
            huddle: vec![chat::HuddleMember {
                party: chat::Party::Account(42),
                node: vec![1; 32],
                joined_at: 0,
            }],
            voice: true,
            owner: chat::Party::Account(7),
            archived: false,
            revision: 1,
        };
        assert!(channel_roster(channel.clone(), 8).is_err());
        let roster = channel_roster(channel.clone(), 7).unwrap();
        assert!(roster.contains(&Caller {
            account: 42,
            node: [1; 32]
        }));
        assert!(!roster.contains(&Caller {
            account: 42,
            node: [2; 32]
        }));
        channel.huddle[0].node.pop();
        assert!(channel_roster(channel, 7).is_err());
    }

    #[test]
    fn authority_requires_secret_identity_and_the_installed_route() {
        let valid = headers();
        assert_eq!(
            caller(&valid, &[b'a'; 64], &config()),
            Some(Caller {
                account: 42,
                node: [1; 32]
            })
        );
        for name in [
            "x-duck-upstream-token",
            "x-duck-caller-account",
            "x-duck-caller-node",
            "x-duck-route-account",
            "x-duck-route-label",
            "x-duck-route-revision",
        ] {
            let mut missing = valid.clone();
            missing.remove(name);
            assert_eq!(caller(&missing, &[b'a'; 64], &config()), None, "{name}");
        }
        for (name, value) in [
            ("x-duck-upstream-token", "b".repeat(64)),
            ("x-duck-caller-node", "xx".repeat(32)),
            ("x-duck-route-account", "8".into()),
            ("x-duck-route-label", "elsewhere".into()),
            ("x-duck-route-revision", "0".into()),
        ] {
            let mut altered = valid.clone();
            altered.insert(name, value.parse().unwrap());
            assert_eq!(caller(&altered, &[b'a'; 64], &config()), None, "{name}");
        }
    }

    struct TestAuthority {
        roster: Arc<Mutex<Roster>>,
        /// Committed chat events exactly as the node's feed delivers them, so
        /// a seat opened here runs the same [`room_change`] filter a deployed
        /// one does.
        changes: broadcast::Sender<ducktape_rpc::ModuleEvent>,
        /// The room of every re-read, as the seat enters it. A test waits on
        /// this instead of a clock, and an event that re-read nothing leaves
        /// nothing here for a later one to hide behind.
        entered: broadcast::Sender<String>,
        /// Whether a re-read answers at once. Held shut, every seat parks
        /// inside its refresh — which is the pause — until a test opens it,
        /// and the answer is the roster as it stands when it opens.
        answering: tokio::sync::watch::Sender<bool>,
        /// The canonical roster becoming unreachable, which is a different
        /// answer from "you were removed" and must end the session just the
        /// same: forwarding under a roster nobody can confirm is the one
        /// outcome the admission rule exists to prevent.
        unreachable: Arc<AtomicBool>,
    }

    impl TestAuthority {
        fn seating(roster: Roster) -> Arc<Self> {
            Arc::new(Self {
                roster: Arc::new(Mutex::new(roster)),
                changes: broadcast::channel(16).0,
                entered: broadcast::channel(16).0,
                answering: tokio::sync::watch::channel(true).0,
                unreachable: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    impl Authority for TestAuthority {
        fn refresh(&self, room: String) -> BoxFuture<'static, Result<Roster, String>> {
            let _ = self.entered.send(room);
            if self.unreachable.load(Ordering::Acquire) {
                return Box::pin(async { Err("room unavailable".into()) });
            }
            let roster = self.roster.clone();
            let mut answering = self.answering.subscribe();
            Box::pin(async move {
                loop {
                    let open = *answering.borrow_and_update();
                    if open || answering.changed().await.is_err() {
                        break;
                    }
                }
                let roster = roster.lock().unwrap().clone();
                Ok(roster)
            })
        }

        fn open(&self, room: String) -> BoxFuture<'static, Result<Membership, String>> {
            let receiver = self.changes.subscribe();
            let roster = self.roster.lock().unwrap().clone();
            let changes = futures::stream::unfold(receiver, move |mut receiver| {
                let room = room.clone();
                async move {
                    loop {
                        let event = match receiver.recv().await {
                            Ok(event) => event,
                            Err(error) => return Some((Err(error.to_string()), receiver)),
                        };
                        if let Some(change) = room_change(&event, &room) {
                            return Some((change, receiver));
                        }
                    }
                }
            })
            .boxed();
            Box::pin(async move { Ok(Membership { roster, changes }) })
        }
    }

    /// One committed chat op, as the node's event feed delivers it.
    fn committed(message: &chat::ChatMsg) -> ducktape_rpc::ModuleEvent {
        ducktape_rpc::ModuleEvent::Changed {
            module: chat::DEFAULT_CHAT_TARGET.into(),
            cursor: "1".into(),
            op: Box::new(ducktape_rpc::StreamOp {
                height: 1,
                seq: 0,
                time: 0,
                origin: ducktape_rpc::StreamOrigin {
                    kind: ducktape_rpc::StreamOriginKind::External,
                    id: None,
                },
                payload: Some(serde_json::to_value(message).expect("a chat write serializes")),
                payload_hex: None,
                assigned: None,
                assigned_hex: None,
            }),
        }
    }

    /// The committed op that takes an account out of a room's huddle.
    fn swept(room: &str, account: u64) -> ducktape_rpc::ModuleEvent {
        committed(&chat::ChatMsg::SweepHuddle {
            channel_id: room.into(),
            party: chat::Party::Account(account),
        })
    }

    /// The committed op a busy workspace produces constantly: a message in
    /// some channel, which for every channel but one changes no roster.
    fn posted(room: &str) -> ducktape_rpc::ModuleEvent {
        committed(&chat::ChatMsg::PostMessage {
            channel_id: room.into(),
            message_id: "m".into(),
            blocks: Vec::new(),
            thread: None,
        })
    }

    /// Which committed chat events reach the roster gate at all. The gate
    /// itself is unchanged: everything that cannot be attributed to another
    /// channel still re-reads.
    #[test]
    fn a_chat_change_re_reads_only_the_room_it_names() {
        assert!(room_change(&posted("room"), "room").is_some());
        assert!(room_change(&posted("other-room"), "room").is_none());
        // Huddle writes are the ones that move a roster, and each names its
        // channel like every other chat write.
        assert!(room_change(&swept("room", 42), "room").is_some());
        assert!(room_change(&swept("other-room", 42), "room").is_none());

        // Unattributable changes re-read: a payload this service cannot read
        // may be the one that revoked the seat.
        let mut opaque = posted("other-room");
        let ducktape_rpc::ModuleEvent::Changed { op, .. } = &mut opaque else {
            panic!("a committed op");
        };
        op.payload = None;
        assert!(room_change(&opaque, "room").is_some());
        // A replay gap lost the ops themselves.
        assert!(
            room_change(
                &ducktape_rpc::ModuleEvent::Lagged {
                    module: chat::DEFAULT_CHAT_TARGET.into(),
                    cursor: "1".into(),
                },
                "room"
            )
            .is_some()
        );

        assert!(room_change(&ducktape_rpc::ModuleEvent::Tip { height: 9 }, "room").is_none());
        assert!(
            room_change(
                &ducktape_rpc::ModuleEvent::Refused {
                    module: chat::DEFAULT_CHAT_TARGET.into(),
                    code: "unindexed".into(),
                },
                "room"
            )
            .is_some_and(|change| change.is_err())
        );
    }

    #[tokio::test]
    async fn real_socket_media_uses_attested_sender_and_revocation_closes_the_session() {
        let first = Caller {
            account: 42,
            node: [1; 32],
        };
        let second = Caller {
            account: 43,
            node: [2; 32],
        };
        let authority =
            TestAuthority::seating([first.clone(), second.clone()].into_iter().collect());
        let service = Arc::new(Service {
            config: config(),
            token: [b'a'; 64],
            authority: authority.clone(),
            hub: Mutex::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/?channel=room", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, service_router(service))
                .await
                .unwrap();
        });
        let request = |caller: &Caller| {
            let mut request = url.clone().into_client_request().unwrap();
            request.headers_mut().extend(headers());
            request.headers_mut().insert(
                "x-duck-caller-account",
                caller.account.to_string().parse().unwrap(),
            );
            request
                .headers_mut()
                .insert("x-duck-caller-node", peer(&caller.node).parse().unwrap());
            request
        };
        let mut denied = request(&first);
        denied
            .headers_mut()
            .insert("x-duck-upstream-token", "b".repeat(64).parse().unwrap());
        let error = tokio_tungstenite::connect_async(denied).await.unwrap_err();
        assert!(
            matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::UNAUTHORIZED)
        );
        let impostor = Caller {
            account: 42,
            node: [9; 32],
        };
        let error = tokio_tungstenite::connect_async(request(&impostor))
            .await
            .unwrap_err();
        assert!(
            matches!(error, tokio_tungstenite::tungstenite::Error::Http(response) if response.status() == StatusCode::FORBIDDEN)
        );

        let (mut left, _) = tokio_tungstenite::connect_async(request(&first))
            .await
            .unwrap();
        let (mut right, _) = tokio_tungstenite::connect_async(request(&second))
            .await
            .unwrap();
        for socket in [&mut left, &mut right] {
            let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&ready).unwrap()["type"],
                "ready"
            );
        }
        right.send(ClientMessage::Text(r#"{"type":"beacon","muted":false,"camera_on":true,"sharing":false,"speaking":true}"#.into())).await.unwrap();
        let beacon = left.next().await.unwrap().unwrap().into_text().unwrap();
        let beacon: serde_json::Value = serde_json::from_str(&beacon).unwrap();
        assert_eq!(beacon["account"], second.account);
        assert_eq!(beacon["peer"], peer(&second.node));

        let audio = media_service::call_wire::encode_audio(&vec![
                1200;
                media_service::voice::FRAME_SAMPLES
            ]);
        right
            .send(ClientMessage::Binary(audio.clone()))
            .await
            .unwrap();
        let heard = left.next().await.unwrap().unwrap().into_data();
        assert_eq!(heard[0], 4);
        assert_eq!(&heard[1..9], &second.account.to_be_bytes());
        assert_eq!(&heard[9..41], &second.node);
        assert_eq!(&heard[41..], &audio[1..]);

        let captured = media_service::call_wire::CapturedFrame {
            keyframe: true,
            ts_ms: 123,
            data: vec![1, 2, 3],
        };
        right
            .send(ClientMessage::Binary(
                media_service::call_wire::encode_captured(&captured),
            ))
            .await
            .unwrap();
        let picture = left.next().await.unwrap().unwrap().into_data();
        let picture = media_service::call_wire::decode_peer(&picture).unwrap();
        assert_eq!(picture.peer, second.node);
        assert_eq!(picture.data, captured.data);

        authority.roster.lock().unwrap().remove(&second);
        authority
            .changes
            .send(swept("room", second.account))
            .unwrap();
        assert!(matches!(
            right.next().await,
            None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_)))
        ));
        let left_event = left.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&left_event).unwrap()["type"],
            "peer_left"
        );
        server.abort();
    }

    /// Resident and peak-resident kibibytes, or zeroes where procfs is absent.
    fn memory() -> (u64, u64) {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return (0, 0);
        };
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default()
        };
        (field("VmRSS:"), field("VmHWM:"))
    }

    fn seated(hub: &mut Hub, peers: usize) -> (Roster, Vec<Caller>, Vec<u64>, Vec<Outbound>) {
        let callers: Vec<Caller> = (0..peers)
            .map(|index| Caller {
                account: index as u64 + 1,
                node: [index as u8 + 1; 32],
            })
            .collect();
        let roster: Roster = callers.iter().cloned().collect();
        let mut ids = Vec::new();
        let mut queues = Vec::new();
        for caller in &callers {
            let (id, queue) = hub.join("room", caller.clone(), &roster).expect("seat");
            ids.push(id);
            queues.push(queue);
        }
        (roster, callers, ids, queues)
    }

    fn audio_frame() -> Message {
        Message::Binary(
            media_service::call_wire::encode_audio(&vec![1200; media_service::voice::FRAME_SAMPLES])
                .into(),
        )
    }

    fn video_frame(bytes: usize, keyframe: bool) -> Message {
        Message::Binary(
            media_service::call_wire::encode_captured(&media_service::call_wire::CapturedFrame {
                keyframe,
                ts_ms: 0,
                data: vec![7; bytes],
            })
            .into(),
        )
    }

    /// What one seat is holding, across both of its lanes. A dropped media
    /// frame is not held and so is not counted: the queue is bounded now, and
    /// this is what it is bounded to.
    fn queued_bytes(queue: &mut Outbound) -> u64 {
        let mut total = 0;
        let mut held = Vec::new();
        while let Ok(message) = queue.control.try_recv() {
            held.push(message);
        }
        loop {
            match queue.media.try_recv() {
                Ok(message) => held.push(message),
                // What the queue dropped is exactly what it is not holding.
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Empty)
                | Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        for message in &held {
            total += match message {
                Message::Text(text) => text.len() as u64,
                Message::Binary(bytes) => bytes.len() as u64,
                _ => 0,
            };
        }
        total
    }

    /// Per-peer queue growth while exactly one peer stops reading. Frames are
    /// synthetic: this measures service fanout and retention only. It is not a
    /// call-quality result and no real capture device is involved.
    #[test]
    #[ignore = "measurement harness"]
    fn stalled_peer_queue_growth_by_frame_profile() {
        println!("== one peer stops reading, the rest drain ==");
        println!(
            "profile\tpeers\tstall_s\tframes\tstalled_depth\tstalled_kib\trss_delta_kib\toldest_age_s\tegress_kbit_s"
        );
        for (profile, fps, video_bytes) in [
            ("audio_only", 50u64, 0usize),
            ("audio_video_8k", 50, 8 * 1024),
            ("audio_video_64k", 50, 64 * 1024),
        ] {
            for peers in [2usize, 5] {
                for stall_s in [1u64, 10, 60] {
                    let base = memory().0;
                    let mut hub = Hub::default();
                    let (roster, callers, ids, mut queues) = seated(&mut hub, peers);
                    // The stalled peer is index 0; the sender is the last seat.
                    let sender = peers - 1;
                    hub.relay(
                        "room",
                        &callers[sender],
                        ids[sender],
                        &roster,
                        Message::Text(
                            r#"{"type":"beacon","muted":false,"camera_on":true,"sharing":false,"speaking":true}"#
                                .into(),
                        ),
                    )
                    .expect("beacon");
                    let frames = fps * stall_s;
                    let video = (video_bytes > 0).then(|| video_frame(video_bytes, false));
                    let audio = audio_frame();
                    let mut relayed_bytes = 0u64;
                    for _ in 0..frames {
                        for frame in [Some(audio.clone()), video.clone()].into_iter().flatten() {
                            hub.relay("room", &callers[sender], ids[sender], &roster, frame)
                                .expect("relay");
                        }
                        // Everyone except the stalled seat keeps up.
                        for queue in queues.iter_mut().skip(1) {
                            relayed_bytes += queued_bytes(queue);
                        }
                    }
                    // Read residency before draining: the drain is what frees it.
                    let depth = queues[0].media.len() + queues[0].control.len();
                    let rss = memory().0;
                    let stalled = queued_bytes(&mut queues[0]);
                    let egress = relayed_bytes * 8 / stall_s.max(1) / 1000;
                    println!(
                        "{profile}\t{peers}\t{stall_s}\t{frames}\t{depth}\t{}\t{}\t{stall_s}\t{egress}",
                        stalled / 1024,
                        rss.saturating_sub(base)
                    );
                    drop(hub);
                }
            }
        }
    }

    /// Fanout cost per frame against participant count, and whether a queued
    /// frame costs memory once or once per recipient.
    #[test]
    #[ignore = "measurement harness"]
    fn fanout_cost_and_frame_sharing_by_participant_count() {
        const FRAMES: u64 = 2_000;
        const FRAME_BYTES: usize = 64 * 1024;
        println!("== fanout with every peer stalled ==");
        println!("peers\tframes\trelay_p50_ns\trelay_p99_ns\tdistinct_mib\tsum_of_queues_mib\trss_delta_kib");
        for peers in [2usize, 4, 8, 16] {
            let base = memory().0;
            let mut hub = Hub::default();
            let (roster, callers, ids, mut queues) = seated(&mut hub, peers);
            let sender = peers - 1;
            hub.relay(
                "room",
                &callers[sender],
                ids[sender],
                &roster,
                Message::Text(
                    r#"{"type":"beacon","muted":false,"camera_on":true,"sharing":false,"speaking":true}"#
                        .into(),
                ),
            )
            .expect("beacon");
            let mut samples = Vec::with_capacity(FRAMES as usize);
            for _ in 0..FRAMES {
                let frame = video_frame(FRAME_BYTES, false);
                let start = std::time::Instant::now();
                hub.relay("room", &callers[sender], ids[sender], &roster, frame)
                    .expect("relay");
                samples.push(start.elapsed().as_nanos() as u64);
            }
            let rss = memory().0;
            samples.sort_unstable();
            let percentile = |percent: usize| samples[(samples.len() * percent / 100).min(samples.len() - 1)];
            let queued: u64 = queues.iter_mut().map(queued_bytes).sum();
            println!(
                "{peers}\t{FRAMES}\t{}\t{}\t{:.1}\t{:.1}\t{}",
                percentile(50),
                percentile(99),
                (FRAMES as f64 * (FRAME_BYTES + media_service::call_wire::WS_VIDEO_PEER_HEADER) as f64)
                    / (1024.0 * 1024.0),
                queued as f64 / (1024.0 * 1024.0),
                rss.saturating_sub(base)
            );
            drop(hub);
        }
    }

    #[tokio::test]
    async fn queued_output_is_preserved_and_old_teardown_cannot_remove_a_new_seat() {
        let first = Caller {
            account: 1,
            node: [1; 32],
        };
        let second = Caller {
            account: 2,
            node: [2; 32],
        };
        let roster = [first.clone(), second.clone()].into_iter().collect();
        let mut hub = Hub::default();
        let (first_id, mut output) = hub.join("room", first.clone(), &roster).unwrap();
        let (second_id, _other) = hub.join("room", second.clone(), &roster).unwrap();
        let audio = Message::Binary(
            media_service::call_wire::encode_audio(&vec![1; media_service::voice::FRAME_SAMPLES])
                .into(),
        );
        for _ in 0..64 {
            hub.relay("room", &second, second_id, &roster, audio.clone())
                .unwrap();
        }
        assert!(matches!(
            output.control.recv().await,
            Some(Message::Text(_))
        ));
        for _ in 0..64 {
            assert!(matches!(output.media.recv().await, Ok(Message::Binary(_))));
        }
        drop(output);
        hub.relay("room", &second, second_id, &roster, audio)
            .unwrap();
        let (replacement, _output) = hub.join("room", first.clone(), &roster).unwrap();
        hub.leave("room", &first, first_id);
        assert_eq!(hub.rooms["room"][&first.account].id, replacement);
        let spoof = Message::Text(r#"{"type":"beacon","peer":"forged","muted":false,"camera_on":false,"sharing":false,"speaking":false}"#.into());
        assert!(
            hub.relay("room", &second, second_id, &roster, spoof)
                .is_err()
        );
    }

    /// One audio frame this test can tell from every other.
    fn numbered_audio(index: usize) -> Message {
        let mut pcm = vec![0i16; media_service::voice::FRAME_SAMPLES];
        pcm[0] = index as i16;
        Message::Binary(media_service::call_wire::encode_audio(&pcm).into())
    }

    /// That number back out of a forwarded frame: tag, account, node, then
    /// the sender's PCM verbatim.
    fn audio_number(message: &Message) -> i16 {
        let Message::Binary(bytes) = message else {
            panic!("a forwarded audio frame");
        };
        i16::from_le_bytes([bytes[41], bytes[42]])
    }

    /// A reader that stops taking frames is served the NEWEST of what it
    /// missed when it resumes, and told how much it lost — not handed a
    /// backlog whose head is a minute old and whose tail is the only part its
    /// three-frame jitter buffer can use. Control frames are not media and
    /// survive the flood: a late reader still knows who is in the room.
    #[test]
    fn a_stalled_reader_resumes_on_the_newest_frames_and_the_drop_is_counted() {
        let talker = Caller {
            account: 1,
            node: [1; 32],
        };
        let stalled = Caller {
            account: 2,
            node: [2; 32],
        };
        let roster: Roster = [talker.clone(), stalled.clone()].into_iter().collect();
        let mut hub = Hub::default();
        let (id, _talker_queue) = hub.join("room", talker.clone(), &roster).expect("seat");
        let (_, mut queue) = hub.join("room", stalled, &roster).expect("seat");

        // Three queues' worth with nobody reading: what survives can only be
        // the tail.
        let sent = QUEUE_FRAMES * 3;
        for index in 0..sent {
            hub.relay("room", &talker, id, &roster, numbered_audio(index))
                .expect("relay");
        }

        let Err(broadcast::error::TryRecvError::Lagged(dropped)) = queue.media.try_recv() else {
            panic!("a stalled reader is told what it lost, not handed the backlog");
        };
        assert_eq!(
            dropped as usize,
            sent - QUEUE_FRAMES,
            "the count is the diagnosis"
        );

        let mut resumed = Vec::new();
        while let Ok(message) = queue.media.try_recv() {
            resumed.push(audio_number(&message));
        }
        assert_eq!(resumed.len(), QUEUE_FRAMES);
        assert_eq!(
            resumed.first().copied(),
            Some((sent - QUEUE_FRAMES) as i16),
            "a resumed reader starts on recent audio, not on the oldest frame it missed"
        );
        assert_eq!(resumed.last().copied(), Some((sent - 1) as i16));

        assert!(
            matches!(queue.control.try_recv(), Ok(Message::Text(_))),
            "the admission roster is not media and is never dropped to make room"
        );
    }

    /// The #2237 membership and lifetime rows that need no capture device:
    /// what a seat's CONTROL state permits the service to forward, and that a
    /// second seat for the same person is refused outright so no call can open
    /// a duplicate transport.
    #[test]
    fn control_state_gates_forwarded_media_and_a_seat_is_never_opened_twice() {
        let talker = Caller {
            account: 1,
            node: [1; 32],
        };
        let listener = Caller {
            account: 2,
            node: [2; 32],
        };
        let roster: Roster = [talker.clone(), listener.clone()].into_iter().collect();
        let mut hub = Hub::default();
        let (id, _talker_queue) = hub.join("room", talker.clone(), &roster).expect("seat");
        let (_, mut heard) = hub.join("room", listener.clone(), &roster).expect("seat");
        assert!(
            matches!(heard.control.try_recv(), Ok(Message::Text(_))),
            "admission sends the roster before any media"
        );

        let beacon = |muted: bool, camera_on: bool| {
            Message::Text(
                serde_json::json!({"type": "beacon", "muted": muted, "camera_on": camera_on,
                "sharing": false, "speaking": true})
                .to_string()
                .into(),
            )
        };

        // Muted: the control frame is forwarded, the voice behind it is not,
        // and the service refuses to describe a muted seat as speaking.
        hub.relay("room", &talker, id, &roster, beacon(true, false))
            .expect("beacon");
        let Ok(Message::Text(forwarded)) = heard.control.try_recv() else {
            panic!("a beacon is forwarded");
        };
        let forwarded: serde_json::Value = serde_json::from_str(&forwarded).unwrap();
        assert_eq!(forwarded["muted"], true);
        assert_eq!(
            forwarded["speaking"], false,
            "a muted seat is never forwarded as speaking"
        );
        hub.relay("room", &talker, id, &roster, audio_frame())
            .expect("accepted and dropped");
        assert!(
            heard.media.try_recv().is_err(),
            "a muted seat's voice is not forwarded"
        );

        // Camera off: a picture frame is accepted from the socket and dropped
        // rather than forwarded, so nothing from the old source lingers.
        hub.relay("room", &talker, id, &roster, video_frame(64, true))
            .expect("accepted and dropped");
        assert!(
            heard.media.try_recv().is_err(),
            "a seat that is not capturing publishes no picture"
        );

        // Unmuted with the camera on: both planes flow again.
        hub.relay("room", &talker, id, &roster, beacon(false, true))
            .expect("beacon");
        assert!(matches!(heard.control.try_recv(), Ok(Message::Text(_))));
        hub.relay("room", &talker, id, &roster, audio_frame())
            .expect("relay");
        assert!(matches!(heard.media.try_recv(), Ok(Message::Binary(_))));
        hub.relay("room", &talker, id, &roster, video_frame(64, true))
            .expect("relay");
        assert!(matches!(heard.media.try_recv(), Ok(Message::Binary(_))));

        // Turning it back off stops the picture plane on the very next frame.
        hub.relay("room", &talker, id, &roster, beacon(false, false))
            .expect("beacon");
        assert!(matches!(heard.control.try_recv(), Ok(Message::Text(_))));
        hub.relay("room", &talker, id, &roster, video_frame(64, true))
            .expect("accepted and dropped");
        assert!(
            heard.media.try_recv().is_err(),
            "the previous source's picture does not outlive the beacon that ended it"
        );

        // A seat is one account AND one node, and neither half may be reused
        // while the seat is held: a second panel or a second transport for the
        // same person is refused rather than seated alongside the first.
        assert!(hub.join("room", talker.clone(), &roster).is_err());
        assert!(
            hub.join(
                "room",
                Caller {
                    account: 99,
                    node: talker.node,
                },
                &roster
            )
            .is_err(),
            "the seated node cannot be re-seated under another account"
        );
        assert!(
            hub.join(
                "room",
                Caller {
                    account: talker.account,
                    node: [9; 32],
                },
                &roster
            )
            .is_err(),
            "the seated account cannot be re-seated from another node"
        );
    }

    /// The canonical roster becoming UNREADABLE is a different answer from
    /// "you were removed", and it ends the session just the same. Forwarding
    /// under a membership nobody can confirm is the one outcome the admission
    /// rule exists to prevent.
    #[tokio::test]
    async fn a_canonical_roster_that_cannot_be_read_ends_the_session() {
        let seated = Caller {
            account: 42,
            node: [1; 32],
        };
        let authority = TestAuthority::seating([seated.clone()].into_iter().collect());
        let service = Arc::new(Service {
            config: config(),
            token: [b'a'; 64],
            authority: authority.clone(),
            hub: Mutex::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/?channel=room", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, service_router(service)).await;
        });
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().extend(headers());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ready).unwrap()["type"],
            "ready"
        );

        // The seat is still in the roster. Only the ability to READ it is lost.
        authority.unreachable.store(true, Ordering::Release);
        authority
            .changes
            .send(swept("room", seated.account))
            .unwrap();
        assert!(matches!(
            socket.next().await,
            None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_)))
        ));
        assert!(
            authority.roster.lock().unwrap().contains(&seated),
            "the session ended because the roster was unreadable, not because it changed"
        );
        server.abort();
    }

    /// Re-reading the canonical roster pauses this seat's media until the
    /// answer lands, and that pause is the authorization gate: nothing is
    /// forwarded under a membership that is being replaced. Chat is ONE
    /// module, so every committed message in the workspace used to buy that
    /// pause — a media stall driven by traffic that cannot have changed who
    /// is in this call. What narrowed is which events reach the gate.
    #[tokio::test]
    async fn another_rooms_chat_change_never_pauses_and_this_rooms_waits_for_the_answer() {
        let first = Caller {
            account: 42,
            node: [1; 32],
        };
        let second = Caller {
            account: 43,
            node: [2; 32],
        };
        let authority =
            TestAuthority::seating([first.clone(), second.clone()].into_iter().collect());
        let service = Arc::new(Service {
            config: config(),
            token: [b'a'; 64],
            authority: authority.clone(),
            hub: Mutex::default(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/?channel=room", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, service_router(service)).await;
        });
        let request = |caller: &Caller| {
            let mut request = url.clone().into_client_request().unwrap();
            request.headers_mut().extend(headers());
            request.headers_mut().insert(
                "x-duck-caller-account",
                caller.account.to_string().parse().unwrap(),
            );
            request
                .headers_mut()
                .insert("x-duck-caller-node", peer(&caller.node).parse().unwrap());
            request
        };
        let (mut left, _) = tokio_tungstenite::connect_async(request(&first))
            .await
            .unwrap();
        let (mut right, _) = tokio_tungstenite::connect_async(request(&second))
            .await
            .unwrap();
        for socket in [&mut left, &mut right] {
            let ready = socket.next().await.unwrap().unwrap().into_text().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&ready).unwrap()["type"],
                "ready"
            );
        }

        // From here nothing answers a re-read: a seat that re-reads parks
        // inside it, and a seat that does not keeps forwarding.
        let mut entered = authority.entered.subscribe();
        authority.answering.send_replace(false);

        // A message in another channel cannot have moved this room's huddle.
        // This frame arriving is the absence of a pause.
        authority.changes.send(posted("other-room")).unwrap();
        let audio = media_service::call_wire::encode_audio(&vec![
                1200;
                media_service::voice::FRAME_SAMPLES
            ]);
        left.send(ClientMessage::Binary(audio.clone()))
            .await
            .unwrap();
        let heard = right.next().await.unwrap().unwrap().into_data();
        assert_eq!(heard[0], 4);
        assert_eq!(&heard[1..9], &first.account.to_be_bytes());

        // This room's huddle changes, and every seat in it re-reads. The
        // first re-read of the whole run is this one: the other room's
        // message bought no node query at all.
        authority.roster.lock().unwrap().remove(&second);
        authority
            .changes
            .send(swept("room", second.account))
            .unwrap();
        assert_eq!(entered.recv().await.unwrap(), "room");
        assert_eq!(entered.recv().await.unwrap(), "room");

        // Both seats are parked inside a re-read that has not answered. A
        // frame sent into that pause is never forwarded under the roster
        // being replaced: by the time the seat runs again the answer has
        // taken the recipient out of the room.
        left.send(ClientMessage::Binary(audio)).await.unwrap();
        authority.answering.send_replace(true);
        loop {
            match right.next().await {
                None | Some(Err(_)) | Some(Ok(ClientMessage::Close(_))) => break,
                Some(Ok(ClientMessage::Binary(_))) => {
                    panic!("media forwarded to a seat whose roster was still being read")
                }
                Some(Ok(_)) => continue,
            }
        }
        let left_event = left.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&left_event).unwrap()["type"],
            "peer_left"
        );
        server.abort();
    }

    // ---- #2237: the reliable transport under impairment ----------------
    //
    // ONE LEG, ONE CLOCK. A sequence number and the send instant ride inside
    // the PCM payload the service forwards verbatim, so both ends are timed by
    // the same monotonic clock and the clock-synchronisation error is zero by
    // construction. This is the guest-to-guest TRANSPORT delay and it is not
    // mouth-to-ear: no capture device and no playout device is opened here,
    // and no loopback or virtual device stands in for one.
    //
    // The impairment is applied OUTSIDE this process, by `tc netem` on the
    // loopback of a dedicated network namespace that holds both endpoints. The
    // harness only measures; it never reaches for `sudo`.

    /// Sequence and send-nanos, as the first twelve bytes of a PCM frame.
    const STAMP_BYTES: usize = 12;

    fn stamp(seq: u32, nanos: u64) -> [u8; STAMP_BYTES] {
        let mut bytes = [0; STAMP_BYTES];
        bytes[..4].copy_from_slice(&seq.to_le_bytes());
        bytes[4..].copy_from_slice(&nanos.to_le_bytes());
        bytes
    }

    fn stamped(bytes: &[u8]) -> (u32, u64) {
        let seq = u32::from_le_bytes(bytes[..4].try_into().expect("stamped sequence"));
        let nanos = u64::from_le_bytes(bytes[4..STAMP_BYTES].try_into().expect("stamped instant"));
        (seq, nanos)
    }

    fn stamped_audio(seq: u32, nanos: u64) -> Vec<u8> {
        let mut pcm = vec![0i16; media_service::voice::FRAME_SAMPLES];
        for (word, pair) in pcm.iter_mut().zip(stamp(seq, nanos).chunks_exact(2)) {
            *word = i16::from_le_bytes([pair[0], pair[1]]);
        }
        media_service::call_wire::encode_audio(&pcm)
    }

    fn stamped_video(seq: u32, nanos: u64, bytes: usize) -> Vec<u8> {
        let mut data = vec![7; bytes.max(STAMP_BYTES)];
        data[..STAMP_BYTES].copy_from_slice(&stamp(seq, nanos));
        media_service::call_wire::encode_captured(&media_service::call_wire::CapturedFrame {
            keyframe: seq.is_multiple_of(50),
            ts_ms: 0,
            data,
        })
    }

    /// One frame's transport leg, in nanoseconds on the single shared clock.
    struct Delivery {
        seq: u32,
        sent: u64,
        received: u64,
    }

    /// What one impairment cell produced. Every field is a count, a byte sum,
    /// or a duration on that one clock.
    #[derive(Default)]
    struct Cell {
        sent: u64,
        deliveries: Vec<Delivery>,
        ended_early: Option<String>,
    }

    fn percentile(sorted: &[u64], percent: usize) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        sorted[(sorted.len() * percent / 100).min(sorted.len() - 1)]
    }

    const MILLIS: u64 = 1_000_000;
    /// One audio frame period. The shipped guest pops exactly one frame per
    /// `clock.ticks` at this period, so it is the natural unit of excess.
    const FRAME_PERIOD_NS: u64 = 20 * MILLIS;
    /// The deployed guest's jitter buffer, `protocol::JITTER_FRAMES` = 3
    /// frames. It lives in the views workspace, which this crate cannot depend
    /// on, so it is restated here — change it there and this goes stale.
    ///
    /// An inter-arrival gap longer than this starves playout by the
    /// difference. That is a LOWER bound: it assumes the buffer was full when
    /// the gap began, which holds right after a burst (the queue caps at three
    /// and drops the oldest) and overstates the buffer in steady flow, where it
    /// hovers nearer one frame. Reported as a bound, never as a mouth-to-ear
    /// silence.
    const JITTER_DEPTH_NS: u64 = 3 * FRAME_PERIOD_NS;
    /// How long a reader waits on a 50 fps source before calling it finished.
    /// Far above any delay measured here, so it ends a run rather than
    /// truncating one.
    const QUIET: std::time::Duration = std::time::Duration::from_secs(5);

    impl Cell {
        /// Delay, head-of-line excess, and the arrival gaps that would starve
        /// the guest's jitter buffer. Ordered by sequence, because excess is a
        /// statement about frames queued BEHIND an earlier one.
        fn report(&mut self, label: &str, kind: &str) {
            self.deliveries.sort_unstable_by_key(|item| item.seq);
            let delays: Vec<u64> = self
                .deliveries
                .iter()
                .map(|item| item.received.saturating_sub(item.sent))
                .collect();
            let mut sorted = delays.clone();
            sorted.sort_unstable();
            let floor = sorted.first().copied().unwrap_or_default();
            let mut longest_run = 0u64;
            let mut longest_run_excess = 0u64;
            let mut run = 0u64;
            let mut run_excess = 0u64;
            let mut max_excess = 0u64;
            for delay in &delays {
                let excess = delay.saturating_sub(floor);
                max_excess = max_excess.max(excess);
                if excess > FRAME_PERIOD_NS {
                    run += 1;
                    run_excess += excess;
                    if run > longest_run {
                        longest_run = run;
                        longest_run_excess = run_excess;
                    }
                    continue;
                }
                run = 0;
                run_excess = 0;
            }
            // How fast the backlog GROWS. A reliable transport with no rate
            // control of its own cannot shed a source that outruns it, so the
            // delay does not settle at a plateau — it climbs for as long as
            // the trial runs. Least squares over (send instant, delay) states
            // that climb as milliseconds of added delay per second of call,
            // which is the figure a fixed p99 taken over one trial length
            // hides.
            let samples = self.deliveries.len() as f64;
            let mean_sent = self
                .deliveries
                .iter()
                .map(|item| item.sent as f64)
                .sum::<f64>()
                / samples.max(1.0);
            let mean_delay = delays.iter().map(|delay| *delay as f64).sum::<f64>() / samples.max(1.0);
            let mut covariance = 0.0;
            let mut variance = 0.0;
            for (item, delay) in self.deliveries.iter().zip(&delays) {
                let offset = item.sent as f64 - mean_sent;
                covariance += offset * (*delay as f64 - mean_delay);
                variance += offset * offset;
            }
            let growth_ms_per_s = match variance > 0.0 {
                true => covariance / variance * 1_000.0,
                false => 0.0,
            };
            let mut starved_gaps = 0u64;
            let mut starved_ns = 0u64;
            let mut worst_gap = 0u64;
            for pair in self.deliveries.windows(2) {
                let gap = pair[1].received.saturating_sub(pair[0].received);
                worst_gap = worst_gap.max(gap);
                if gap > JITTER_DEPTH_NS {
                    starved_gaps += 1;
                    starved_ns += gap - JITTER_DEPTH_NS;
                }
            }
            let millis = |nanos: u64| nanos as f64 / MILLIS as f64;
            let received = self.deliveries.len() as u64;
            println!(
                "{label}\t{kind}\t{}\t{received}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{longest_run}\t{:.2}\t{growth_ms_per_s:.1}\t{starved_gaps}\t{:.2}\t{:.2}\t{}",
                self.sent,
                self.sent.saturating_sub(received),
                millis(percentile(&sorted, 50)),
                millis(percentile(&sorted, 95)),
                millis(percentile(&sorted, 99)),
                millis(sorted.last().copied().unwrap_or_default()),
                millis(max_excess),
                millis(longest_run_excess),
                millis(starved_ns),
                millis(worst_gap),
                self.ended_early.as_deref().unwrap_or("-"),
            );
        }
    }

    fn env_number(name: &str, fallback: u64) -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(fallback)
    }

    /// Delivery delay and head-of-line accumulation over the real reliable
    /// WebSocket, through the shipped `Hub`. Impairment comes from `tc netem`
    /// in the namespace this process runs in; the label names the cell.
    ///
    /// Synthetic sources throughout. This is NOT a call-quality verdict — the
    /// capture and playout stages are absent, and #2237's device half stays
    /// unverified for want of a microphone and a camera.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "measurement harness"]
    async fn impaired_transport_delivery_and_head_of_line_accumulation() {
        let label = std::env::var("DUCKTAPE_CALL_CELL").unwrap_or_else(|_| "unlabelled".into());
        let trial = std::time::Duration::from_millis(env_number("DUCKTAPE_CALL_TRIAL_MS", 20_000));
        let video_bytes = env_number("DUCKTAPE_CALL_VIDEO_BYTES", 0) as usize;
        let stalled_peer = env_number("DUCKTAPE_CALL_STALLED_PEER", 0) == 1;
        let warmup = std::time::Duration::from_secs(1);

        let sender = Caller {
            account: 42,
            node: [1; 32],
        };
        let reader = Caller {
            account: 43,
            node: [2; 32],
        };
        let stalled = Caller {
            account: 44,
            node: [3; 32],
        };
        let mut seats = vec![sender.clone(), reader.clone()];
        if stalled_peer {
            seats.push(stalled.clone());
        }
        let authority = TestAuthority::seating(seats.iter().cloned().collect());
        let service = Arc::new(Service {
            config: config(),
            token: [b'a'; 64],
            authority,
            hub: Mutex::default(),
        });
        // `DUCKTAPE_CALL_NAGLE=1` restores the batching both legs had before
        // `realtime_listener`, so a cell can be measured before and after the
        // fix on one binary under one impairment. The default is the shipped
        // path.
        let nagle = env_number("DUCKTAPE_CALL_NAGLE", 0) == 1;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/?channel=room", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let router = service_router(service);
            let _ = match nagle {
                true => axum::serve(listener, router).await,
                false => axum::serve(realtime_listener(listener), router).await,
            };
        });
        let connect = |caller: Caller| {
            let url = url.clone();
            async move {
                use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
                let mut request = url.into_client_request().unwrap();
                request.headers_mut().extend(headers());
                request.headers_mut().insert(
                    "x-duck-caller-account",
                    caller.account.to_string().parse().unwrap(),
                );
                request
                    .headers_mut()
                    .insert("x-duck-caller-node", peer(&caller.node).parse().unwrap());
                let (socket, _) =
                    tokio_tungstenite::connect_async_with_config(request, None, !nagle)
                        .await
                        .unwrap();
                socket
            }
        };
        let mut publisher = connect(sender).await;
        let listening = connect(reader).await;
        let _stalled = match stalled_peer {
            true => Some(connect(stalled).await),
            false => None,
        };

        // A camera beacon: the shipped `relay` refuses picture frames from a
        // seat that is not capturing, so the video cells need it before the
        // first frame and the audio cells are unaffected by it.
        publisher
            .send(ClientMessage::Text(
                r#"{"type":"beacon","muted":false,"camera_on":true,"sharing":false,"speaking":true}"#
                    .into(),
            ))
            .await
            .unwrap();

        println!(
            "cell\tkind\tsent\treceived\tlost\tp50_ms\tp95_ms\tp99_ms\tmax_ms\tmax_excess_ms\thol_run_frames\thol_run_excess_ms\tgrowth_ms_per_s\tstarved_gaps\tstarved_ms\tworst_gap_ms\tended_early"
        );
        let clock = std::time::Instant::now();
        let receiving = tokio::spawn(async move {
            let mut audio = Cell::default();
            let mut video = Cell::default();
            let mut socket = listening;
            loop {
                // Two terminal conditions, and the harness needs both.
                //
                // `peer_left` is the clean one: it rides the SAME FIFO queue
                // the relayed frames went through, so every frame a
                // head-of-line stall was still holding has landed by the time
                // it does.
                //
                // QUIESCENCE is the backstop, and it is not a disguised
                // timeout on a result. `netem` can wedge a loopback connection
                // outright — observed with 58,950 bytes sent, never acked, and
                // no retransmission for twelve minutes — and then the clean
                // event never arrives at all. The source is a real-time paced
                // stream, so silence this long IS its end; whatever never
                // arrived is counted as lost, with the reason recorded.
                let Ok(frame) = tokio::time::timeout(QUIET, socket.next()).await else {
                    audio.ended_early = Some("reader_quiesced".into());
                    break;
                };
                let Some(frame) = frame else { break };
                let now = clock.elapsed().as_nanos() as u64;
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        audio.ended_early = Some(error.to_string());
                        break;
                    }
                };
                // The publisher's seat leaving is this reader's terminal event.
                // `leave` pushes `peer_left` onto the SAME FIFO queue the
                // relayed frames went through, so every frame a head-of-line
                // stall was still holding has already arrived when it lands —
                // which is why this harness never waits on a clock to decide
                // the tail is in.
                let bytes = match frame {
                    ClientMessage::Binary(bytes) => bytes,
                    ClientMessage::Text(text) if text.contains("peer_left") => break,
                    _ => continue,
                };
                let payload = match bytes.first() {
                    Some(4) if bytes.len() >= 41 + STAMP_BYTES => &bytes[41..],
                    Some(3) if bytes.len() >= 38 + STAMP_BYTES => &bytes[38..],
                    _ => continue,
                };
                let cell = match bytes[0] {
                    4 => &mut audio,
                    _ => &mut video,
                };
                let (seq, sent) = stamped(payload);
                cell.deliveries.push(Delivery {
                    seq,
                    sent,
                    received: now,
                });
            }
            (audio, video)
        });

        // Paced against a monotonic deadline schedule, so the source does not
        // drift and the impairment is what the delay measures. The pacing IS
        // the workload here; no assertion below waits on a clock.
        let mut audio_seq = 0u32;
        let mut video_seq = 0u32;
        let mut audio_sent_live = 0u64;
        let mut video_sent_live = 0u64;
        let mut failure = None;
        let deadline = clock + warmup + trial;
        while std::time::Instant::now() < deadline {
            let due = clock
                + std::time::Duration::from_nanos(FRAME_PERIOD_NS * u64::from(audio_seq));
            tokio::time::sleep_until(due.into()).await;
            let counted = clock.elapsed() >= warmup;
            let now = clock.elapsed().as_nanos() as u64;
            if let Err(error) = publisher
                .send(ClientMessage::Binary(stamped_audio(audio_seq, now)))
                .await
            {
                failure = Some(error.to_string());
                break;
            }
            if counted {
                audio_sent_live += 1;
            }
            audio_seq += 1;
            // 10 fps video on the same socket, when the cell asks for it.
            let video_due = video_bytes > 0 && audio_seq.is_multiple_of(5);
            if video_due {
                if let Err(error) = publisher
                    .send(ClientMessage::Binary(
                        stamped_video(video_seq, now, video_bytes),
                    ))
                    .await
                {
                    failure = Some(error.to_string());
                    break;
                }
                if counted {
                    video_sent_live += 1;
                }
                video_seq += 1;
            }
        }
        drop(publisher);
        let (mut audio, mut video) = receiving.await.expect("reader task");
        audio.sent = audio_sent_live;
        video.sent = video_sent_live;
        if audio.ended_early.is_none() {
            audio.ended_early = failure;
        }
        // Frames sent during warm-up are excluded from both sides of the
        // ledger, so a warm-up delivery is not counted as an extra arrival.
        audio
            .deliveries
            .retain(|item| item.sent >= warmup.as_nanos() as u64);
        video
            .deliveries
            .retain(|item| item.sent >= warmup.as_nanos() as u64);
        audio.report(&label, "audio");
        if video_bytes > 0 {
            video.report(&label, "video");
        }
        server.abort();
    }
}
