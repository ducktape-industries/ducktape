//! Deployed call protocol over generic media devices and Gateway streams.
mod protocol;
mod session;

use ducktape_view_guest::{Subscription, Task, wire};

pub struct CallView;
#[derive(Clone)]
pub enum Message {
    Progress,
}

impl CallView {
    const PREFERRED_WINDOW_SIZE: &'static str = "none";
    fn boot() -> (Self, Task<Message>) {
        (Self, Task::none())
    }
    fn view(&self) -> wire::Node {
        wire::Node::empty()
    }
    fn update(&mut self, _: Message) -> Task<Message> {
        Task::none()
    }
    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(session::run)
    }
    // Replacement restarts protocol resources against current properties.
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn restore(bytes: &[u8]) -> Result<Self, String> {
        if !bytes.is_empty() {
            return Err("call snapshot must be empty".into());
        }
        Ok(Self)
    }
}

ducktape_view_guest::export_app!(
    CallView,
    "Call",
    "Background audio and video session",
    ["call", "media", "net", "rpc", "host", "clock"]
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    struct Host {
        guest: ducktape_view_guest::Driver<CallView>,
        streams: BTreeMap<String, u64>,
        effects: Vec<(String, Value)>,
    }

    impl Host {
        fn new() -> Self {
            Self {
                guest: ducktape_view_guest::Driver::new(),
                streams: BTreeMap::new(),
                effects: Vec::new(),
            }
        }
        fn response(id: u64, value: Value, done: bool) -> wire::Event {
            wire::Event::Response {
                id,
                result: Ok(serde_json::to_vec(&value).unwrap()),
                done,
            }
        }
        fn step(&mut self, mut events: Vec<wire::Event>) {
            loop {
                let frame = self.guest.tick(events);
                events = Vec::new();
                for request in frame.requests {
                    match request.kind.as_str() {
                        "call.props" | "media.audio" | "media.video" | "clock.ticks" => {
                            self.streams.insert(request.kind, request.id);
                        }
                        "rpc.query" => {
                            let query: Value = serde_json::from_slice(&request.payload).unwrap();
                            assert_eq!(query["query"]["channel"]["channel_id"], "room");
                            events.push(Self::response(
                                request.id,
                                json!({"channel": {"owner": {"account": 7}}}),
                                true,
                            ));
                        }
                        "net.stream" => {
                            let route: Value = serde_json::from_slice(&request.payload).unwrap();
                            assert_eq!(route["account"], 7);
                            assert_eq!(route["route"], "media");
                            assert_eq!(route["path"], "/?channel=room");
                            self.streams.insert(request.kind, request.id);
                            events.push(Self::response(
                                request.id,
                                json!({"text": r#"{"type":"ready","peers":[]}"#}),
                                false,
                            ));
                        }
                        "media.image" => events.push(Self::response(
                            request.id,
                            json!({"image": 99, "key": "opaque-image"}),
                            true,
                        )),
                        _ => {
                            let payload: Value = serde_json::from_slice(&request.payload).unwrap();
                            self.effects.push((request.kind, payload));
                            events.push(wire::Event::Response {
                                id: request.id,
                                result: Ok(Vec::new()),
                                done: true,
                            });
                        }
                    }
                }
                if events.is_empty() && !frame.busy {
                    break;
                }
            }
        }
        fn item(&mut self, stream: &str, value: Value) {
            self.step(vec![Self::response(self.streams[stream], value, false)]);
        }
    }

    #[test]
    fn real_guest_contract_routes_media_and_changes_sources_without_native_call_protocol() {
        let mut host = Host::new();
        host.step(Vec::new());
        host.item(
            "call.props",
            json!({"channel": "room", "muted": false, "source": "off"}),
        );
        assert!(host.streams.contains_key("net.stream"));
        host.item(
            "media.audio",
            json!({"samples": vec![1200; protocol::SAMPLES]}),
        );
        assert!(
            host.effects
                .iter()
                .any(|(kind, body)| kind == "net.send" && body["frame"]["binary"][0] == 1)
        );

        let peer = "02".repeat(32);
        host.item("net.stream", json!({"text": json!({"type":"peer_beacon","account":43,"peer":peer,"muted":false,"camera_on":true,"sharing":false,"speaking":true}).to_string()}));
        let mut audio = vec![4];
        audio.extend_from_slice(&43u64.to_be_bytes());
        audio.extend_from_slice(&[2; 32]);
        for _ in 0..protocol::SAMPLES {
            audio.extend_from_slice(&500i16.to_le_bytes());
        }
        host.item("net.stream", json!({"binary": audio}));
        host.step(vec![wire::Event::Response {
            id: host.streams["clock.ticks"],
            result: Ok(Vec::new()),
            done: false,
        }]);
        assert!(
            host.effects
                .iter()
                .any(|(kind, body)| kind == "media.play" && body["samples"][0] == 500)
        );

        host.item(
            "call.props",
            json!({"channel": "room", "muted": true, "source": "screen"}),
        );
        host.item(
            "media.video",
            json!({"timestamp_ms": 7, "jpeg": [8,9], "preview":"local-preview"}),
        );
        assert!(host.effects.iter().any(|(kind, body)| kind == "net.send"
            && body["frame"]["binary"] == json!([2, 1, 0, 0, 0, 7, 8, 9])));
        let before = host.effects.len();
        host.item(
            "media.audio",
            json!({"samples": vec![1200; protocol::SAMPLES]}),
        );
        assert!(
            !host.effects[before..]
                .iter()
                .any(|(kind, body)| kind == "net.send" && body["frame"]["binary"][0] == 1)
        );

        let mut video = vec![3, 1, 0, 0, 0, 7];
        video.extend_from_slice(&[2; 32]);
        video.push(9);
        host.item("net.stream", json!({"binary": video}));
        assert!(
            host.effects
                .iter()
                .any(|(kind, body)| kind == "media.put" && body["image"] == 99)
        );
        assert!(
            host.effects
                .iter()
                .any(|(kind, body)| kind == "host.emit"
                    && body["peers"][0]["image"] == "opaque-image")
        );
    }
    #[test]
    fn guest_selects_the_stage_and_emits_only_presentation_changes() {
        fn shown(host: &Host) -> Value {
            host.effects
                .iter()
                .rev()
                .find(|(kind, body)| kind == "host.emit" && body["kind"] == "presentation")
                .expect("guest presentation")
                .1
                .clone()
        }
        let mut host = Host::new();
        host.step(Vec::new());
        host.item("call.props", json!({"channel":"room", "source":"off"}));
        assert_eq!(
            shown(&host),
            json!({"kind":"presentation", "stage":"", "video_live":false, "peers":[]})
        );
        host.item("call.props", json!({"channel":"room", "source":"screen"}));
        host.item(
            "media.video",
            json!({"timestamp_ms":1, "jpeg":[9], "preview":"local-preview"}),
        );
        assert_eq!(shown(&host)["stage"], "local-preview");
        assert_eq!(shown(&host)["video_live"], true);
        let peer = "02".repeat(32);
        host.item(
            "net.stream",
            json!({"text":json!({"type":"peer_beacon", "peer":peer, "sharing":true}).to_string()}),
        );
        assert_eq!(
            shown(&host)["stage"],
            "local-preview",
            "a beacon alone has no image to show"
        );
        let mut video = vec![3, 1, 0, 0, 0, 7];
        video.extend_from_slice(&[2; 32]);
        video.push(9);
        host.item("net.stream", json!({"binary":video}));
        assert_eq!(
            shown(&host)["stage"],
            "opaque-image",
            "remote share takes priority"
        );
        let before = host
            .effects
            .iter()
            .filter(|(_, body)| body["kind"] == "presentation")
            .count();
        host.item(
            "media.video",
            json!({"timestamp_ms":2, "jpeg":[9], "preview":"local-preview"}),
        );
        assert_eq!(
            host.effects
                .iter()
                .filter(|(_, body)| body["kind"] == "presentation")
                .count(),
            before
        );
        host.item(
            "net.stream",
            json!({"text":json!({"type":"peer_left", "peer":peer}).to_string()}),
        );
        assert_eq!(shown(&host)["stage"], "local-preview");
        host.item("call.props", json!({"channel":"room", "source":"off"}));
        assert_eq!(
            shown(&host),
            json!({"kind":"presentation", "stage":"", "video_live":false, "peers":[]})
        );
    }
    #[test]
    fn guest_presentation_replaces_peer_state_without_native_folding() {
        fn peers(host: &Host) -> Value {
            host.effects
                .iter()
                .rev()
                .find(|(kind, body)| kind == "host.emit" && body["kind"] == "presentation")
                .unwrap()
                .1["peers"]
                .clone()
        }
        let mut host = Host::new();
        host.step(Vec::new());
        host.item("call.props", json!({"channel":"room", "source":"off"}));
        assert_eq!(peers(&host), json!([]));
        let peer = "02".repeat(32);
        for muted in [true, false] {
            host.item("net.stream", json!({"text":json!({"type":"peer_beacon", "peer":peer, "muted":muted, "speaking":true}).to_string()}));
            let list = peers(&host);
            assert_eq!(list.as_array().unwrap().len(), 1);
            assert_eq!(list[0]["peer"], peer);
            assert_eq!(list[0]["muted"], muted);
            assert_eq!(list[0]["speaking"], true);
        }
        let before = host.effects.len();
        host.item("net.stream", json!({"text":json!({"type":"peer_beacon", "peer":peer, "muted":false, "speaking":true}).to_string()}));
        assert!(
            !host.effects[before..]
                .iter()
                .any(|(_, body)| body["kind"] == "presentation")
        );
        host.item(
            "net.stream",
            json!({"text":json!({"type":"peer_left", "peer":peer}).to_string()}),
        );
        assert_eq!(peers(&host), json!([]));
    }
}
