//! Execute protocol effects through the ordinary view host contract.
use std::collections::BTreeMap;

use ducktape_view_guest::host;
use futures::{FutureExt as _, StreamExt as _, stream::LocalBoxStream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Message;
use crate::protocol::{self, Beacon, Effect, Event, Machine, Props};

/// One item off a host subscription, or `None` when the stream ended. The
/// refusal is converted to its sentence at [`answer`], the one place this view
/// leaves the host's shape — nothing below branches on a token.
type Answer = Option<host::Answer>;

enum Input {
    Props(Answer),
    Network(Answer),
    Audio(Answer),
    Video(Answer),
    Clock(Answer),
}
enum Run {
    Start,
    Panel(host::Subscription),
    Live(Box<Session>),
    End,
}

struct Session {
    properties: host::Subscription,
    network: host::Subscription,
    audio: host::Subscription,
    video: Option<host::Subscription>,
    clock: host::Subscription,
    machine: Machine,
    images: BTreeMap<String, (u64, String)>,
    preview: String,
    presentation: Option<Presentation>,
    /// The newest peer state the transport has not accepted yet. Beacons are
    /// absolute and last-write-wins, so only the newest one is worth keeping.
    unsent_beacon: Option<Value>,
}

#[derive(PartialEq, Eq, Serialize)]
struct Peer {
    peer: String,
    image: String,
    #[serde(flatten)]
    beacon: Beacon,
}

#[derive(PartialEq, Eq)]
struct Presentation {
    stage: String,
    video_live: bool,
    peers: Vec<Peer>,
    tiles: Vec<String>,
}

fn bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("JSON effect")
}
fn notify(kind: &str, value: Value) {
    host::notify(kind, &bytes(&value));
}
fn emit(value: Value) {
    notify("host.emit", value);
}
fn status(kind: &str, message: &str) {
    emit(json!({"kind": kind, "message": message, "status": status_text(kind, message)}));
}

fn status_text(kind: &str, message: &str) -> String {
    match kind {
        "connecting" | "closed" => kind.into(),
        "live" => {
            if message.is_empty() {
                "live".into()
            } else {
                format!("live · {message}")
            }
        }
        _ => message.into(),
    }
}
fn answer(answer: Answer) -> Result<Vec<u8>, String> {
    answer
        .ok_or_else(|| "session stream closed".to_owned())?
        .map_err(host::said)
}

fn properties(bytes: &[u8]) -> Result<Props, String> {
    let props: Props = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let valid = !props.channel.is_empty()
        && props.channel.len() <= 256
        && matches!(props.source.as_str(), "off" | "camera" | "screen");
    if !valid {
        return Err("invalid call properties".into());
    }
    Ok(props)
}

fn escaped(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

pub fn run() -> LocalBoxStream<'static, Message> {
    futures::stream::unfold(Run::Start, |state| async move {
        let next = match state {
            Run::Start => begin().await,
            Run::Panel(mut properties) => {
                let item = answer(properties.next().await).and_then(|bytes| panel_properties(&bytes));
                item.map(|panel| (Message::Panel(Box::new(panel)), Run::Panel(properties)))
            }
            Run::Live(mut session) => session.advance().await.map(|()| (Message::Progress, Run::Live(session))),
            Run::End => return None,
        };
        let next = match next {
            Ok(next) => next,
            Err(error) => {
                emit(json!({"kind":"presentation", "stage":"", "video_live":false, "peers":[], "tiles":[]}));
                status("error", &error);
                host::notify("host.finish", &[]);
                (Message::Progress, Run::End)
            }
        };
        Some(next)
    })
    .boxed_local()
}

fn panel_properties(bytes: &[u8]) -> Result<crate::panel::Panel, String> {
    #[derive(Deserialize)]
    struct Envelope {
        panel: crate::panel::Panel,
    }
    serde_json::from_slice::<Envelope>(bytes)
        .map(|value| value.panel)
        .map_err(|error| error.to_string())
}

async fn begin() -> Result<(Message, Run), String> {
    let mut stream = host::subscribe("call.props", &[]);
    let bytes = answer(stream.next().await)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if value.get("panel").is_some() {
        return Ok((
            Message::Panel(Box::new(panel_properties(&bytes)?)),
            Run::Panel(stream),
        ));
    }
    let session = Session::connect(stream, properties(&bytes)?).await?;
    Ok((Message::Progress, Run::Live(Box::new(session))))
}

impl Session {
    async fn connect(properties_stream: host::Subscription, props: Props) -> Result<Self, String> {
        status("connecting", "");
        let channel = host::request(
            "rpc.query",
            &bytes(&json!({"target": "chat", "query": {"channel": {"channel_id": props.channel}}})),
        )
        .await
        .map_err(host::said)?;
        let channel: Value = serde_json::from_slice(&channel).map_err(|error| error.to_string())?;
        let owner = channel
            .get("channel")
            .and_then(|channel| channel.get("owner"))
            .and_then(|owner| owner.get("account"))
            .and_then(Value::as_u64)
            .ok_or("room has no account-owned media route")?;
        let mut network = host::subscribe(
            "net.stream",
            &bytes(&json!({"account": owner, "route": "media", "method": "get",
            "path": format!("/?channel={}", escaped(&props.channel)), "headers": [], "body": []})),
        );
        let ready = network_message(&answer(network.next().await)?)?;
        let Network::Text(ready) = ready else {
            return Err("media service sent no ready message".into());
        };
        if ready.get("type").and_then(Value::as_str) != Some("ready") {
            return Err("media service refused session".into());
        }
        let mut session = Self {
            properties: properties_stream,
            network,
            audio: host::subscribe("media.audio", b"{}"),
            video: None,
            clock: host::subscribe("clock.ticks", &20i64.to_le_bytes()),
            machine: Machine::default(),
            images: BTreeMap::new(),
            preview: String::new(),
            presentation: None,
            unsent_beacon: None,
        };
        let initial = session.machine.step(Event::Properties(props));
        session.execute(initial).await?;
        for event in control(ready)? {
            let effects = session.machine.step(event);
            session.execute(effects).await?;
        }
        status("live", "");
        Ok(session)
    }

    async fn advance(&mut self) -> Result<(), String> {
        let input = {
            let properties = self.properties.next().fuse();
            let network = self.network.next().fuse();
            let audio = self.audio.next().fuse();
            let clock = self.clock.next().fuse();
            let video = async {
                match self.video.as_mut() {
                    Some(video) => video.next().await,
                    None => std::future::pending().await,
                }
            }
            .fuse();
            futures::pin_mut!(properties, network, audio, clock, video);
            futures::select_biased! {
                item = properties => Input::Props(item),
                item = network => Input::Network(item),
                item = clock => Input::Clock(item),
                item = audio => Input::Audio(item),
                item = video => Input::Video(item),
            }
        };
        let events = self.events(input)?;
        for event in events {
            let effects = self.machine.step(event);
            self.execute(effects).await?;
        }
        Ok(())
    }

    fn events(&mut self, input: Input) -> Result<Vec<Event>, String> {
        match input {
            Input::Props(item) => self.changed_properties(item),
            Input::Network(item) => self.received(item),
            Input::Audio(item) => self.captured_audio(item),
            Input::Video(item) => self.captured_video(item),
            Input::Clock(item) => self.clock_tick(item),
        }
    }

    fn changed_properties(&mut self, item: Answer) -> Result<Vec<Event>, String> {
        let props = properties(&answer(item)?)?;
        if props.channel != self.machine.props.channel {
            return Err("call room changed".into());
        }
        Ok(vec![Event::Properties(props)])
    }

    fn received(&mut self, item: Answer) -> Result<Vec<Event>, String> {
        match network_message(&answer(item)?)? {
            Network::Text(value) => control(value),
            Network::Binary(bytes) => protocol::binary(&bytes).map(|event| vec![event]),
        }
    }

    fn captured_audio(&mut self, item: Answer) -> Result<Vec<Event>, String> {
        let value: Value =
            serde_json::from_slice(&answer(item)?).map_err(|error| error.to_string())?;
        if value.get("ready") == Some(&Value::Bool(true)) {
            status(
                "live",
                value
                    .get("note")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            );
            return Ok(Vec::new());
        }
        // the device hands over an ENCODED frame and its own verdict on
        // whether it carried sound: the samples the verdict was measured on
        // never leave the host.
        let frame =
            serde_json::from_value(value.get("frame").cloned().ok_or("missing voice frame")?)
                .map_err(|error| error.to_string())?;
        let sound = value
            .get("sound")
            .and_then(Value::as_bool)
            .unwrap_or_default();
        Ok(vec![Event::LocalAudio { frame, sound }])
    }

    fn captured_video(&mut self, item: Answer) -> Result<Vec<Event>, String> {
        #[derive(Deserialize)]
        struct Image {
            timestamp_ms: u32,
            jpeg: Vec<u8>,
            preview: String,
        }
        let bytes = match answer(item) {
            Ok(bytes) => bytes,
            Err(error) => {
                status("live", &error);
                self.video.take();
                let mut props = self.machine.props.clone();
                props.source = "off".into();
                return Ok(vec![Event::Properties(props)]);
            }
        };
        let image: Image = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        self.preview = image.preview;
        Ok(vec![Event::LocalImage {
            timestamp_ms: image.timestamp_ms,
            jpeg: image.jpeg,
        }])
    }

    fn clock_tick(&mut self, item: Answer) -> Result<Vec<Event>, String> {
        answer(item)?;
        Ok(vec![Event::Tick])
    }

    async fn execute(&mut self, effects: Vec<Effect>) -> Result<(), String> {
        // A beacon an earlier turn could not hand over goes out ahead of this
        // turn's effects, so peer state still reaches the room in the order
        // the machine decided it.
        self.flush_beacon().await;
        for effect in effects {
            match effect {
                Effect::SendText(text) => self.send_beacon(text).await,
                Effect::SendBinary(binary) => notify(
                    "net.send",
                    json!({"stream": self.network.id(), "frame": {"binary": binary}}),
                ),
                Effect::SelfState(state) => emit(
                    json!({"kind": "self", "muted": state.muted, "camera_on": state.camera_on, "sharing": state.sharing, "speaking": state.speaking}),
                ),
                Effect::Capture(source) => self.capture(&source),
                Effect::Mute(muted) => notify(
                    "media.mute",
                    json!({"audio": self.audio.id(), "muted": muted}),
                ),
                Effect::Play(due) => {
                    let frames: Vec<Value> = due
                        .into_iter()
                        .map(|(peer, frame)| json!({"peer": peer, "frame": frame}))
                        .collect();
                    notify(
                        "media.play",
                        json!({"audio": self.audio.id(), "frames": frames}),
                    )
                }
                Effect::Image { peer, jpeg } => self.picture(peer, jpeg).await?,
                Effect::DropImage(peer) => self.drop_picture(&peer),
            }
        }
        self.present();
        Ok(())
    }

    async fn send_beacon(&mut self, text: Value) {
        self.unsent_beacon = Some(text);
        self.flush_beacon().await;
    }

    /// Peer state is absolute and last-write-wins, so a transport that REFUSES
    /// to queue a beacon keeps the newest one and offers it again on the next
    /// turn rather than ending the call. A congested writer refuses exactly
    /// this way, and losing the room because a control frame arrived a tick
    /// late is a worse outcome than the late tick.
    ///
    /// A transport that is genuinely gone still ends the session: the network
    /// subscription delivers its own terminal item and `received` fails on it.
    /// Every turn emits `Effect::Play`, so the retry runs at the playout tick.
    async fn flush_beacon(&mut self) {
        let Some(text) = self.unsent_beacon.take() else {
            return;
        };
        let queued = host::request(
            "net.send",
            &bytes(&json!({"stream": self.network.id(), "frame": {"text": text.to_string()}})),
        )
        .await;
        if queued.is_err() {
            self.unsent_beacon = Some(text);
        }
    }

    fn present(&mut self) {
        let remote = self.machine.peers.iter().find_map(|(peer, beacon)| {
            if !beacon.sharing {
                return None;
            }
            self.images.get(peer).map(|(_, key)| key.clone())
        });
        let stage = remote.unwrap_or_else(|| match self.machine.props.source.as_str() {
            "screen" => self.preview.clone(),
            _ => String::new(),
        });
        let local_video = matches!(self.machine.props.source.as_str(), "camera" | "screen");
        let remote_video = self
            .machine
            .peers
            .values()
            .any(|peer| peer.camera_on || peer.sharing);
        let peers = self
            .machine
            .peers
            .iter()
            .map(|(peer, beacon)| Peer {
                peer: peer.clone(),
                image: self
                    .images
                    .get(peer)
                    .map(|(_, key)| key.clone())
                    .unwrap_or_default(),
                beacon: beacon.clone(),
            })
            .collect();
        let mut tiles: Vec<String> = self
            .images
            .values()
            .map(|(_, key)| key)
            .filter(|key| **key != stage)
            .cloned()
            .collect();
        let show_preview = local_video && !self.preview.is_empty() && self.preview != stage;
        if show_preview {
            tiles.push(self.preview.clone());
        }
        let current = Presentation {
            stage,
            video_live: local_video || remote_video,
            peers,
            tiles,
        };
        let unchanged = self.presentation.as_ref() == Some(&current);
        if unchanged {
            return;
        }
        emit(
            json!({"kind":"presentation", "stage":current.stage, "video_live":current.video_live, "peers":current.peers, "tiles":current.tiles}),
        );
        self.presentation = Some(current);
    }

    fn capture(&mut self, source: &str) {
        self.video.take();
        self.preview.clear();
        if source != "off" {
            self.video = Some(host::subscribe(
                "media.video",
                &bytes(&json!({"source": source, "max_bytes": 65_530})),
            ));
        }
    }

    async fn picture(&mut self, peer: String, jpeg: Vec<u8>) -> Result<(), String> {
        if !self.images.contains_key(&peer) {
            #[derive(Deserialize)]
            struct Image {
                image: u64,
                key: String,
            }
            let image = host::request("media.image", b"{}")
                .await
                .map_err(host::said)?;
            let image: Image = serde_json::from_slice(&image).map_err(|error| error.to_string())?;
            self.images.insert(peer.clone(), (image.image, image.key));
        }
        notify(
            "media.put",
            json!({"image": self.images[&peer].0, "jpeg": jpeg}),
        );
        Ok(())
    }

    fn drop_picture(&mut self, peer: &str) {
        if let Some((image, _)) = self.images.remove(peer) {
            notify("media.drop", json!({"image": image}));
        }
    }
}

enum Network {
    Text(Value),
    Binary(Vec<u8>),
}

fn network_message(bytes: &[u8]) -> Result<Network, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return serde_json::from_str(text)
            .map(Network::Text)
            .map_err(|error| error.to_string());
    }
    let binary = value
        .get("binary")
        .cloned()
        .ok_or("unknown network frame")?;
    serde_json::from_value(binary)
        .map(Network::Binary)
        .map_err(|error| error.to_string())
}

fn control(value: Value) -> Result<Vec<Event>, String> {
    fn peer(value: &Value) -> Result<String, String> {
        let peer = value
            .get("peer")
            .and_then(Value::as_str)
            .ok_or("missing peer")?;
        let canonical = peer.len() == 64
            && peer
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        canonical
            .then(|| peer.to_owned())
            .ok_or_else(|| "invalid peer".into())
    }
    match value.get("type").and_then(Value::as_str) {
        Some("ready") => {
            let peers = value
                .get("peers")
                .and_then(Value::as_array)
                .ok_or("missing room roster")?;
            if peers.len() > 32 {
                return Err("room roster exceeds limit".into());
            }
            peers
                .iter()
                .map(|item| {
                    Ok(Event::Peer {
                        peer: peer(item)?,
                        beacon: serde_json::from_value(
                            item.get("state").cloned().ok_or("missing peer state")?,
                        )
                        .map_err(|error| error.to_string())?,
                    })
                })
                .collect()
        }
        Some("peer_beacon") => Ok(vec![Event::Peer {
            peer: peer(&value)?,
            beacon: serde_json::from_value(value).map_err(|error| error.to_string())?,
        }]),
        Some("peer_left") => Ok(vec![Event::Left(peer(&value)?)]),
        _ => Err("unknown call control".into()),
    }
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    #[test]
    fn status_labels_belong_to_the_deployed_session() {
        assert_eq!(status_text("connecting", ""), "connecting");
        assert_eq!(status_text("live", ""), "live");
        assert_eq!(status_text("live", "no microphone"), "live · no microphone");
        assert_eq!(status_text("refused", "not seated"), "not seated");
        assert_eq!(status_text("closed", ""), "closed");
    }
}
