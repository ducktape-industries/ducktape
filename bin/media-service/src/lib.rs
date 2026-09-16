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
use tokio::sync::mpsc;

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
                .filter_map(|event| async move {
                    match event {
                        Ok(
                            ducktape_rpc::ModuleEvent::Changed { .. }
                            | ducktape_rpc::ModuleEvent::Lagged { .. },
                        ) => Some(Ok(())),
                        Ok(
                            ducktape_rpc::ModuleEvent::Ready { .. }
                            | ducktape_rpc::ModuleEvent::Tip { .. },
                        ) => None,
                        Ok(ducktape_rpc::ModuleEvent::Refused { .. }) | Err(_) => {
                            Some(Err("room event feed lost".into()))
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

struct Participant {
    id: u64,
    caller: Caller,
    output: mpsc::UnboundedSender<Message>,
    beacon: Beacon,
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
    ) -> Result<(u64, mpsc::UnboundedReceiver<Message>), String> {
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
        let (output, input) = mpsc::unbounded_channel();
        let _ = output.send(Message::Text(
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
                output,
                beacon: Beacon::default(),
            },
        );
        Ok((id, input))
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
        participants.retain(|_, participant| participant.output.send(left.clone()).is_ok());
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
            !recipient || participant.output.send(message.clone()).is_ok()
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

async fn serve(
    socket: WebSocket,
    seat: Seat,
    mut membership: Membership,
    mut output: mpsc::UnboundedReceiver<Message>,
) {
    let (mut sink, mut incoming) = socket.split();
    let writer = async {
        while let Some(outgoing) = output.recv().await {
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
    use tokio_tungstenite::tungstenite::{
        Message as ClientMessage, client::IntoClientRequest as _,
    };

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
        changes: tokio::sync::broadcast::Sender<()>,
    }

    impl Authority for TestAuthority {
        fn refresh(&self, _: String) -> BoxFuture<'static, Result<Roster, String>> {
            let roster = self.roster.lock().unwrap().clone();
            Box::pin(async move { Ok(roster) })
        }

        fn open(&self, _: String) -> BoxFuture<'static, Result<Membership, String>> {
            let receiver = self.changes.subscribe();
            let roster = self.roster.lock().unwrap().clone();
            let changes = futures::stream::unfold(receiver, |mut receiver| async move {
                let event = receiver.recv().await.map_err(|error| error.to_string());
                Some((event, receiver))
            })
            .boxed();
            Box::pin(async move { Ok(Membership { roster, changes }) })
        }
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
        let authority = Arc::new(TestAuthority {
            roster: Arc::new(Mutex::new(
                [first.clone(), second.clone()].into_iter().collect(),
            )),
            changes: tokio::sync::broadcast::channel(8).0,
        });
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
        authority.changes.send(()).unwrap();
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
        assert!(matches!(output.recv().await, Some(Message::Text(_))));
        for _ in 0..64 {
            assert!(matches!(output.recv().await, Some(Message::Binary(_))));
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
}
