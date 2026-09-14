use ducktape_view_guest::{Subscription, Task, wire};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PagesEditorFixture {
    formatting_notice: String,
    #[serde(with = "document_snapshot")]
    document: ducktape_view_guest::Editor,
    history: crate::editor_binding::HistoryState,
    menu: crate::editor_binding::MenuState,
    source: crate::fixture_source::DocumentSource,
    installed_source: Vec<u8>,
    load_error: String,
    paint_dark: bool,
    commented: Vec<i64>,
}

impl std::fmt::Debug for PagesEditorFixture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PagesEditorFixture")
    }
}

#[derive(Clone)]
pub enum Message {
    Load,
    DocumentArrived(crate::fixture_source::DocumentItem),
    Committed(crate::editor_binding::EditorUpdate),
    DocumentUpdated(ducktape_view_guest::EditorDocumentUpdate),
    DocumentTransaction(ducktape_view_guest::EditorTransaction<Message>),
}
impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Message")
    }
}

impl PagesEditorFixture {
    pub(crate) const PREFERRED_WINDOW_SIZE: &'static str = "none";
    const SNAPSHOT_SCHEMA: &'static str =
        "9892d966d395d7a36d84bf74ccb9098e147486b1bf3f865c0eeb35fd5a2d1961";
    fn boot() -> (Self, Task<Message>) {
        (
            Self {
                formatting_notice: String::new(),
                document: ducktape_view_guest::Editor::new("- 한글"),
                history: crate::editor_binding::initial_history(),
                menu: crate::editor_binding::initial_menu(),
                source: crate::fixture_source::empty_source(),
                installed_source: Vec::new(),
                load_error: String::new(),
                paint_dark: false,
                commented: Vec::new(),
            },
            Task::none(),
        )
    }

    fn snapshot(&self) -> Result<Vec<u8>, String> {
        wire::Snapshot {
            schema: Self::SNAPSHOT_SCHEMA.into(),
            state: wire::SnapshotValue::Bytes(wire::encode(self)),
        }
        .encode()
    }

    fn restore(bytes: &[u8]) -> Result<Self, String> {
        let snapshot = wire::Snapshot::decode(bytes)?;
        if snapshot.schema != Self::SNAPSHOT_SCHEMA {
            return Err("invalid fixture snapshot schema".into());
        }
        let wire::SnapshotValue::Bytes(state) = snapshot.state else {
            return Err("invalid fixture snapshot".into());
        };
        wire::decode(&state)
    }

    fn subscription(&self) -> Subscription<Message> {
        let needs_source =
            !self.source.reference.is_empty() && self.source.reference != self.installed_source;
        if needs_source {
            crate::fixture_source::document_source(self.source.clone())
                .map(Message::DocumentArrived)
        } else {
            Subscription::none()
        }
    }
}

mod document_snapshot {
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(
        editor: &ducktape_view_guest::Editor,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        editor.snapshot().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ducktape_view_guest::Editor, D::Error> {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        ducktape_view_guest::Editor::restore(&bytes)
            .ok_or_else(|| serde::de::Error::custom("invalid editor snapshot"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn view_and_snapshot_keep_fixture_document() {
        let (app, _) = PagesEditorFixture::boot();
        let _ = app.view();
        let snapshot = app.snapshot().unwrap();
        let restored = PagesEditorFixture::restore(&snapshot).unwrap();
        assert_eq!(restored.document.text(), "- 한글");
        assert_eq!(restored.snapshot().unwrap(), snapshot);
    }
}
include!("app_update.rs");
include!("app_view.rs");
