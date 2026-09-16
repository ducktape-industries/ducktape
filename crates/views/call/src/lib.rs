//! Deployed call protocol over generic media devices and Gateway streams.
mod panel;
mod protocol;
mod session;

use ducktape_view_guest::{Subscription, Task, wire};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct CallView {
    panel: Option<panel::Panel>,
    invited: std::collections::BTreeSet<String>,
    room: panel::Room,
    error: String,
    #[serde(skip)]
    invitations: std::collections::BTreeMap<String, ducktape_view_guest::task::Handle>,
}
#[derive(Clone)]
pub enum Message {
    Progress,
    Panel(Box<panel::Panel>),
    Action(panel::Action),
    InviteFinished(panel::RoomKey, String, Result<(), String>),
    RoomLoaded(panel::RoomKey, Result<panel::Room, String>),
}

impl CallView {
    const PREFERRED_WINDOW_SIZE: &'static str = "none";
    fn boot() -> (Self, Task<Message>) {
        (Self::default(), Task::none())
    }
    fn view(&self) -> wire::Node {
        self.panel.as_ref().map_or_else(wire::Node::empty, |panel| {
            panel.view(&self.room, &self.invited, &self.error)
        })
    }
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Progress => self.on_progress(),
            Message::Panel(panel) => self.on_panel(*panel),
            Message::Action(action) => self.on_action(action),
            Message::RoomLoaded(key, result) => self.on_room_loaded(key, result),
            Message::InviteFinished(instance, key, result) => {
                self.on_invite_finished(instance, key, result)
            }
        }
    }
    fn on_progress(&mut self) -> Task<Message> {
        Task::none()
    }
    fn on_panel(&mut self, panel: panel::Panel) -> Task<Message> {
        let changed = self.panel.as_ref().is_none_or(|previous| {
            previous.room_key() != panel.room_key() || previous.joined != panel.joined
        });
        if changed {
            self.invitations.clear();
            self.invited.clear();
            self.room = panel::Room::default();
            self.error.clear();
        }
        self.panel = Some(panel);
        Task::none()
    }
    fn on_room_loaded(
        &mut self,
        key: panel::RoomKey,
        result: Result<panel::Room, String>,
    ) -> Task<Message> {
        let current = self
            .panel
            .as_ref()
            .is_some_and(|panel| panel.joined && panel.room_key() == key);
        if !current {
            return Task::none();
        }
        match result {
            Ok(room) => self.room = room,
            Err(error) => self.error = error,
        }
        Task::none()
    }
    fn on_action(&mut self, action: panel::Action) -> Task<Message> {
        match action {
            panel::Action::Mute => self.control("call.mute"),
            panel::Action::Camera => self.control("call.camera"),
            panel::Action::Screen => self.control("call.screen"),
            panel::Action::Channel => self.control("call.channel"),
            panel::Action::Leave => self.control("call.leave"),
            panel::Action::Invite(key) => self.invite(key),
        }
    }
    fn control(&self, kind: &str) -> Task<Message> {
        panel::notify(kind);
        Task::none()
    }
    fn invite(&mut self, key: String) -> Task<Message> {
        let Some(panel) = &self.panel else {
            return Task::none();
        };
        let available = panel.joined
            && !panel.loading
            && !panel.channel.is_empty()
            && self.room.members.iter().any(|member| member.key == key);
        if !available || !self.invited.insert(key.clone()) {
            return Task::none();
        }
        self.error.clear();
        let room = panel.room_key();
        let member = key.clone();
        let (task, handle) = Task::perform(
            panel::invite(panel.channel.clone(), self.room.title.clone(), key.clone()),
            move |result| Message::InviteFinished(room.clone(), key.clone(), result),
        )
        .abortable();
        self.invitations.insert(member, handle.abort_on_drop());
        task
    }
    fn on_invite_finished(
        &mut self,
        room: panel::RoomKey,
        key: String,
        result: Result<(), String>,
    ) -> Task<Message> {
        let current = self
            .panel
            .as_ref()
            .is_some_and(|panel| panel.joined && panel.room_key() == room);
        if !current {
            return Task::none();
        }
        self.invitations.remove(&key);
        if let Err(error) = result {
            self.invited.remove(&key);
            self.error = error;
        }
        Task::none()
    }
    fn subscription(&self) -> Subscription<Message> {
        let protocol = Subscription::run(session::run);
        let Some(panel) = &self.panel else {
            return protocol;
        };
        if !panel.joined || panel.channel.is_empty() {
            return protocol;
        }
        Subscription::batch([protocol, panel::rooms(panel.room_key())])
    }
    // Protocol resources restart; panel interaction state survives replacement.
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }
    fn restore(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
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

    #[test]
    fn panel_replacement_preserves_invites_and_rejects_an_old_rooms_reply() {
        let mut view = CallView::default();
        let _ = view.on_panel(panel::Panel {
            instance: 7,
            joined: true,
            channel: "room".into(),
            ..Default::default()
        });
        view.invited.insert("acct:8".into());
        let mut restored = CallView::restore(&view.snapshot().unwrap()).unwrap();
        let _ = restored.on_panel(panel::Panel {
            instance: 7,
            joined: true,
            channel: "room".into(),
            muted: true,
            ..Default::default()
        });
        assert!(restored.invited.contains("acct:8"));
        let _ = restored.on_panel(panel::Panel {
            instance: 8,
            joined: true,
            channel: "other".into(),
            ..Default::default()
        });
        restored.invited.insert("acct:8".into());
        let old = view.panel.as_ref().unwrap().room_key();
        let current = restored.panel.as_ref().unwrap().room_key();
        let _ = restored.on_invite_finished(old, "acct:8".into(), Err("old failure".into()));
        assert!(restored.error.is_empty());
        assert!(restored.invited.contains("acct:8"));
        let _ =
            restored.on_invite_finished(current, "acct:8".into(), Err("current failure".into()));
        assert_eq!(restored.error, "current failure");
        assert!(restored.invited.is_empty());
    }

    #[test]
    fn identity_changes_cancel_pending_invites_and_discard_the_old_rooms_state() {
        use futures::{FutureExt as _, StreamExt as _};
        let base = panel::Panel {
            instance: 7,
            channel: "room".into(),
            joined: true,
            network: "chain-a".into(),
            endpoint: "node-a".into(),
            account: "7".into(),
            user_key: "key-a".into(),
            ..Default::default()
        };
        let alternatives = [
            panel::Panel {
                network: "chain-b".into(),
                ..base.clone()
            },
            panel::Panel {
                endpoint: "node-b".into(),
                ..base.clone()
            },
            panel::Panel {
                account: "8".into(),
                ..base.clone()
            },
            panel::Panel {
                user_key: "key-b".into(),
                ..base.clone()
            },
            panel::Panel {
                joined: false,
                ..base.clone()
            },
        ];
        for changed in alternatives {
            let mut view = CallView::default();
            let _ = view.on_panel(base.clone());
            view.room.title = "old room".into();
            view.invited.insert("acct:9".into());
            let (task, handle) = Task::future(std::future::pending::<Message>()).abortable();
            view.invitations
                .insert("acct:9".into(), handle.abort_on_drop());
            let _ = view.on_panel(changed);
            assert!(view.invited.is_empty());
            assert!(view.room.title.is_empty());
            assert!(view.invitations.is_empty());
            assert!(matches!(
                task.into_stream().next().now_or_never(),
                Some(None)
            ));
            let _ = view.on_invite_finished(
                base.room_key(),
                "acct:9".into(),
                Err("old failure".into()),
            );
            let _ = view.on_room_loaded(base.room_key(), Err("old lookup".into()));
            assert!(view.error.is_empty());
        }
    }

    #[test]
    fn room_queries_belong_to_the_current_panel_instance() {
        let mut view = CallView::default();
        let old = panel::Panel {
            instance: 1,
            ..Default::default()
        }
        .room_key();
        let current = panel::Panel {
            instance: 2,
            joined: true,
            ..Default::default()
        };
        let key = current.room_key();
        let _ = view.on_panel(current);
        let _ = view.on_room_loaded(old, Err("old room".into()));
        assert!(view.error.is_empty());
        let _ = view.on_room_loaded(
            key,
            Ok(panel::Room {
                title: "current".into(),
                ..Default::default()
            }),
        );
        assert_eq!(view.room.title, "current");
    }

    #[test]
    fn panel_properties_do_not_start_devices_or_transport() {
        let mut host = Host::new();
        host.step(Vec::new());
        host.item("call.props", json!({"panel": {"title": "Engineering"}}));
        assert_eq!(
            host.streams.keys().map(String::as_str).collect::<Vec<_>>(),
            ["call.props"]
        );
        assert!(host.effects.is_empty());
        host.item(
            "call.props",
            json!({"panel": {"title": "Other room", "muted": true}}),
        );
        assert_eq!(host.streams.len(), 1);
        assert!(host.effects.is_empty());
    }

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
            json!({"kind":"presentation", "stage":"", "video_live":false, "peers":[], "tiles":[]})
        );
        host.item("call.props", json!({"channel":"room", "source":"screen"}));
        host.item(
            "media.video",
            json!({"timestamp_ms":1, "jpeg":[9], "preview":"local-preview"}),
        );
        assert_eq!(shown(&host)["stage"], "local-preview");
        assert_eq!(shown(&host)["tiles"], json!([]));
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
        assert_eq!(shown(&host)["tiles"], json!(["local-preview"]));
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
        assert_eq!(shown(&host)["tiles"], json!([]));
        host.item("call.props", json!({"channel":"room", "source":"camera"}));
        assert_eq!(
            shown(&host)["tiles"],
            json!([]),
            "old capture preview is retired"
        );
        host.item(
            "media.video",
            json!({"timestamp_ms":3, "jpeg":[9], "preview":"camera-preview"}),
        );
        assert_eq!(shown(&host)["stage"], "");
        assert_eq!(shown(&host)["tiles"], json!(["camera-preview"]));
        host.item("call.props", json!({"channel":"room", "source":"off"}));
        assert_eq!(
            shown(&host),
            json!({"kind":"presentation", "stage":"", "video_live":false, "peers":[], "tiles":[]})
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
