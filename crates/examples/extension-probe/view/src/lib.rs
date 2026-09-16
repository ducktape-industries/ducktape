//! An independently deployed view exercising only common host operations.
use ducktape_view_guest::{Task, Subscription, host, kit, slots, wire};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct ExtensionView {
    draft: String,
    output: String,
    connected: bool,
    #[serde(skip)]
    stream: Option<u64>,
}

#[derive(Clone)]
pub enum Message {
    Draft(String),
    Request,
    Record,
    Read,
    Connect,
    Disconnect,
    Send,
    Opened(u64),
    Answer(host::Answer),
}

fn ask(kind: &'static str, body: serde_json::Value) -> Task<Message> {
    Task::perform(
        host::request(kind, &serde_json::to_vec(&body).unwrap()),
        Message::Answer,
    )
}

fn route(method: &str, path: &str, body: Vec<u8>) -> serde_json::Value {
    serde_json::json!({"account":1,"route":"extension-probe","method":method,
        "path":path,"headers":[{"name":"content-type","value":"application/json"}],"body":body})
}

fn display_answer(bytes: &[u8]) -> String {
    use base64::Engine as _;
    let decoded = serde_json::from_slice::<serde_json::Value>(bytes).ok();
    let body = match decoded.as_ref() {
        Some(reply) if reply["body_b64"].is_string() => base64::engine::general_purpose::STANDARD
            .decode(reply["body_b64"].as_str().unwrap())
            .unwrap_or_default(),
        Some(frame) if frame["text"].is_string() => {
            frame["text"].as_str().unwrap().as_bytes().to_vec()
        }
        _ => bytes.to_vec(),
    };
    let reply = serde_json::from_slice::<serde_json::Value>(&body).ok();
    match reply.as_ref().and_then(|reply| reply["reply"].as_str()) {
        Some(text) => text.to_owned(),
        None => String::from_utf8_lossy(&body).into_owned(),
    }
}

impl ExtensionView {
    const PREFERRED_WINDOW_SIZE: &'static str = "none";
    fn boot() -> (Self, Task<Message>) {
        (Self::default(), Task::none())
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Draft(text) => self.draft(text),
            Message::Request => self.request(),
            Message::Record => self.record(),
            Message::Read => self.read(),
            Message::Connect => self.connect(),
            Message::Disconnect => self.disconnect(),
            Message::Send => self.send(),
            Message::Opened(id) => self.opened(id),
            Message::Answer(answer) => self.answer(answer),
        }
    }
    fn draft(&mut self, text: String) -> Task<Message> {
        let bounded = text.len() <= 128;
        if bounded {
            self.draft = text;
        }
        Task::none()
    }
    fn request(&self) -> Task<Message> {
        let body = serde_json::to_vec(&serde_json::json!({"text":self.draft})).unwrap();
        ask("net.request", route("post", "/reply", body))
    }
    fn record(&self) -> Task<Message> {
        ask(
            "op.submit",
            serde_json::json!({"target":"extension-policy","payload":{"record":{"text":self.draft}}}),
        )
    }
    fn read(&self) -> Task<Message> {
        ask(
            "rpc.query",
            serde_json::json!({"target":"extension-policy","query":"state"}),
        )
    }
    fn connect(&mut self) -> Task<Message> {
        self.connected = true;
        Task::none()
    }
    fn disconnect(&mut self) -> Task<Message> {
        self.connected = false;
        self.stream = None;
        Task::none()
    }
    fn opened(&mut self, id: u64) -> Task<Message> {
        self.stream = Some(id);
        Task::none()
    }
    fn send(&self) -> Task<Message> {
        let Some(stream) = self.stream else {
            return Task::none();
        };
        ask(
            "net.send",
            serde_json::json!({"stream":stream,"frame":{"text":serde_json::json!({"text":self.draft}).to_string()}}),
        )
    }
    fn answer(&mut self, answer: host::Answer) -> Task<Message> {
        let acknowledgement = answer.as_ref().is_ok_and(Vec::is_empty);
        if acknowledgement {
            return Task::none();
        }
        self.output = match answer {
            Ok(bytes) => display_answer(&bytes),
            Err(reason) => reason,
        };
        Task::none()
    }
    fn subscription(&self) -> Subscription<Message> {
        Subscription::run_with(self.connected, |connected| {
            if !connected {
                return futures::stream::empty().boxed_local();
            }
            let stream = host::subscribe(
                "net.stream",
                &serde_json::to_vec(&route("get", "/stream", Vec::new())).unwrap(),
            );
            let opened = Message::Opened(stream.id());
            futures::stream::once(std::future::ready(opened))
                .chain(stream.map(Message::Answer))
                .boxed_local()
        })
    }
    fn view(&self) -> wire::Node {
        let title = if cfg!(feature = "replacement") {
            "Extension updated"
        } else {
            "Extension probe"
        };
        let mut rows = vec![
            kit::text("title", title),
            kit::input(
                "draft",
                "Message",
                &self.draft,
                slots::handler(Box::new(|text| Some(Message::Draft(text)))),
                None,
            ),
        ];
        for (key, label, message) in [
            ("request", "HTTP request", Message::Request),
            ("record", "Record", Message::Record),
            ("read", "Read state", Message::Read),
            ("connect", "Connect", Message::Connect),
            ("send", "Send", Message::Send),
            ("disconnect", "Disconnect", Message::Disconnect),
        ] {
            rows.push(kit::button(
                key,
                label,
                Some(slots::message(message)),
                wire::ButtonPreset::Secondary,
            ));
        }
        rows.push(kit::text("output", &self.output));
        kit::column("extension", rows)
    }
    fn snapshot(&self) -> Result<Vec<u8>, String> {
        wire::Snapshot {
            schema: "870cb35f5c2ac891476e0bd7cb035f2219fb956a7bf50249d4a095a16cce72bd".into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }
    fn restore(bytes: &[u8]) -> Result<Self, String> {
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != "870cb35f5c2ac891476e0bd7cb035f2219fb956a7bf50249d4a095a16cce72bd" {
            return Err("unknown snapshot".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid state".into());
        };
        wire::decode(&state)
    }
}

ducktape_view_guest::export_app!(
    ExtensionView,
    "Extension probe",
    "Independent application transport and policy",
    ["net", "rpc", "op"]
);

#[cfg(test)]
mod tests {
    use super::*;
    use ducktape_view_guest::testing::{type_into, press};

    #[test]
    fn input_drives_generic_http_module_and_bidirectional_requests() {
        boot_native();
        let frame = tick_native(Vec::new());
        let frame = tick_native(type_into(&frame, "draft", "#Hello"));
        let frame = tick_native(press(&frame, "request"));
        let request = frame
            .requests
            .iter()
            .find(|request| request.kind == "net.request")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
        assert_eq!(body["account"], 1);
        assert_eq!(body["route"], "extension-probe");
        assert_eq!(body["method"], "post");
        assert_eq!(body["path"], "/reply");
        let bytes: Vec<u8> = serde_json::from_value(body["body"].clone()).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["text"],
            "#Hello"
        );
        assert!(body.get("user_pop").is_none());
        let frame = tick_native(press(&frame, "record"));
        let request = frame
            .requests
            .iter()
            .find(|request| request.kind == "op.submit")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
        assert_eq!(body["payload"]["record"]["text"], "#Hello");
        let frame = tick_native(press(&frame, "connect"));
        let stream = frame
            .requests
            .iter()
            .find(|request| request.kind == "net.stream")
            .unwrap()
            .id;
        let frame = tick_native(press(&frame, "send"));
        let request = frame
            .requests
            .iter()
            .find(|request| request.kind == "net.send")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&request.payload).unwrap();
        assert_eq!(body["stream"], stream);
        assert_eq!(body["frame"]["text"], r##"{"text":"#Hello"}"##);
        let frame = tick_native(press(&frame, "disconnect"));
        assert!(frame.cancels.contains(&stream));
    }

    #[test]
    fn replacement_snapshot_retains_draft_but_never_reuses_native_stream_handle() {
        let app = ExtensionView {
            draft: "kept".into(),
            output: "answer".into(),
            connected: true,
            stream: Some(42),
        };
        let restored = ExtensionView::restore(&app.snapshot().unwrap()).unwrap();
        assert_eq!(restored.draft, "kept");
        assert_eq!(restored.output, "answer");
        assert!(restored.connected);
        assert_eq!(restored.stream, None);
    }
}
